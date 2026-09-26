//! Integration tests for the readiness probe.
//!
//! They run against the development stack
//! (`docker compose -f infra/compose/docker-compose.dev.yml up -d`) — locally and in CI,
//! where the same PostgreSQL and Redis are provided as service containers. When the stack is
//! not running the suite skips itself with a printed reason, so `cargo test` stays usable on
//! a machine without Docker.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use tower::ServiceExt;

/// Local port with nothing listening behind it, used to prove the `503` path.
const DEAD_REDIS_URL: &str = "redis://127.0.0.1:6399";

async fn get(path: &str, state: AppState) -> (StatusCode, serde_json::Value) {
    let response = routes::router(state)
        .oneshot(
            Request::builder()
                .uri(path)
                .body(Body::empty())
                .expect("request must build"),
        )
        .await
        .expect("router must answer");

    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
        .to_bytes();
    let body = serde_json::from_slice(&bytes).expect("body must be JSON");
    (status, body)
}

fn test_state(config: Config, db: Db, redis: RedisClient) -> AppState {
    AppState::new(
        BuildInfo::new("omnion-api", "0.0.0-test"),
        config,
        db,
        redis,
        test_storage(),
    )
}

/// Object store of the test state.
///
/// These suites never touch the object store — that is the media suite's job — so the default
/// development configuration is enough: it opens without contacting anything.
fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
}

/// Connect to the compose PostgreSQL; `None` means the stack is not running.
async fn live_db(config: &Config) -> Option<Db> {
    match Db::connect(&config.database).await {
        Ok(db) => Some(db),
        Err(err) => {
            eprintln!(
                "SKIP: PostgreSQL is not reachable ({err}) — start it with \
                 `docker compose -f infra/compose/docker-compose.dev.yml up -d`"
            );
            None
        }
    }
}

#[tokio::test]
async fn readyz_reports_ok_when_dependencies_answer() {
    let config = Config::from_env().expect("environment must be valid");
    let Some(db) = live_db(&config).await else {
        return;
    };
    db.migrate().await.expect("migrations must apply");

    let redis = RedisClient::new(&config.redis.url).expect("redis URL must parse");
    if let Err(err) = redis.ping().await {
        eprintln!("SKIP: Redis is not reachable ({err}) — start the compose stack");
        return;
    }

    let (status, body) = get("/readyz", test_state(config, db, redis)).await;
    assert_eq!(status, StatusCode::OK, "readyz body: {body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["checks"]["database"]["status"], "ok");
    assert_eq!(body["checks"]["redis"]["status"], "ok");
    assert_eq!(body["service"], "omnion-api");
}

#[tokio::test]
async fn migrations_create_the_initial_schema() {
    let config = Config::from_env().expect("environment must be valid");
    let Some(db) = live_db(&config).await else {
        return;
    };
    db.migrate().await.expect("migrations must apply");

    for table in ["organizations", "users", "sessions", "audit_log"] {
        let exists: bool = sqlx::query_scalar(
            "select exists (select 1 from information_schema.tables \
             where table_schema = 'public' and table_name = $1)",
        )
        .bind(table)
        .fetch_one(db.pool())
        .await
        .expect("catalog query must run");
        assert!(exists, "table `{table}` must exist after migrations");
    }

    let applied: i64 = sqlx::query_scalar("select count(*) from _sqlx_migrations where success")
        .fetch_one(db.pool())
        .await
        .expect("migration bookkeeping must be readable");
    assert!(applied >= 1, "at least one migration must be recorded");
}

#[tokio::test]
async fn readyz_reports_503_when_a_dependency_is_down() {
    let config = Config::from_env().expect("environment must be valid");
    let db = Db::connect_lazy(&config.database).expect("lazy pool must build");
    let redis = RedisClient::new(DEAD_REDIS_URL).expect("dead URL must still parse");

    let (status, body) = get("/readyz", test_state(config, db, redis)).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "readyz body: {body}"
    );
    assert_eq!(body["ok"], false);
    assert_eq!(body["checks"]["redis"]["status"], "unavailable");
}
