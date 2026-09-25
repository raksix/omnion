//! Integration test: the router really answers `/healthz`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

#[tokio::test]
async fn healthz_reports_ok() {
    let response = omnion_api::routes::router()
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
