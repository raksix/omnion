//! Integration test: the router really answers `/healthz`.
//!
//! Liveness must not depend on any dependency being up, so this test uses the default state
//! (lazy, unconnected handles).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use omnion_api::routes;
use omnion_api::state::AppState;
use tower::ServiceExt;

#[tokio::test]
async fn healthz_reports_ok() {
    let response = routes::router(AppState::default())
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .expect("request must build"),
        )
        .await
        .expect("router must answer");

    assert_eq!(response.status(), StatusCode::OK);

    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body must read")
        .to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("body must be JSON");

    assert_eq!(body["ok"], serde_json::Value::Bool(true));
    assert_eq!(body["service"], "omnion-api");
}
