//! `/api/v1/security/headers` — the header policy (REQ-012, slice 2).
//!
//! This is the one endpoint pair in the platform whose *output* leaves the API and lands in
//! every browser the platform serves, so three rules apply to it that do not apply elsewhere:
//!
//! * **The preview and the response are the same rendering.** [`HeaderPolicy::render`] produces
//!   one list, and it is what this module returns in `GET` and what the middleware in
//!   [`crate::headers_middleware`] puts on the wire. A panel that showed a summary while the
//!   middleware applied a different assembly is the "the header I configured is not the header I
//!   get" bug.
//! * **A save validates before it stores, and refuses loudly after.** A policy with an empty
//!   directive name or a source that hides two sources comes back `400` naming the field. The
//!   store's compare-and-swap then refuses a stale form with a message that tells the operator
//!   to reload rather than silently overwriting somebody else's change.
//! * **Nothing here echoes a secret.** The policy is configuration about the deployment, not
//!   about a credential; the CSRF secret that guards mutations is never in this response, in the
//!   history, or in the audit entry the save writes.

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use omnion_audit::{NewAuditEntry, record as record_audit};
use omnion_events::{NewEvent, bus};
use omnion_security::{
    CspDirective, CspMode, HeaderPolicy, HstsPolicy, load_headers, save_headers,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

/// Map a crate error onto the API surface.
fn map_store(error: omnion_security::SecurityError) -> ApiError {
    use omnion_security::SecurityError as E;
    match error {
        E::Invalid(message) => ApiError::bad_request("invalid_security_input", message),
        E::Database(inner) => ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("security settings store: {inner}"),
        ),
        E::NotFound => ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "not_found",
            "header policy not found",
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// One header line as the panel reads it.
///
/// `value: None` is not an omission: it is "this header is configured off", and the panel renders
/// it as a row with a strike rather than hiding it. A header an operator turned off and cannot
/// see is a header they will not know is off.
#[derive(Debug, Serialize)]
pub struct HeaderLineBody {
    /// The header's name.
    pub name: String,
    /// Its value, or `None` when it is not sent.
    pub value: Option<String>,
}

/// The policy as the panel reads it.
#[derive(Debug, Serialize)]
pub struct HeadersBody {
    /// The CSP mode, `report_only` or `enforce`.
    pub csp_mode: String,
    /// The directives, in render order.
    pub csp: Vec<CspDirective>,
    /// HSTS as stored.
    pub hsts: HstsPolicy,
    /// Whether `X-Content-Type-Options: nosniff` is sent.
    pub content_type_options: bool,
    /// The referrer policy, or `None`.
    pub referrer_policy: Option<String>,
    /// Permissions-policy entries.
    pub permissions_policy: Vec<String>,
    /// **The exact header lines a response carries right now.** This is what the preview
    /// column shows and it is produced by the same function the middleware uses.
    pub rendered: Vec<HeaderLineBody>,
    /// Whether the stored document is one somebody has saved, or the platform's baseline that
    /// no operator has chosen yet.
    pub saved: bool,
    /// Who last saved it, if anyone.
    pub updated_by: Option<Uuid>,
    /// When it was last saved, `None` before the first save.
    pub updated_at: Option<String>,
}

/// The save request's shape.
///
/// `expected_document` is the document the form was opened with; it is the compare-and-swap key.
#[derive(Debug, Deserialize)]
pub struct SaveHeadersBody {
    /// `report_only` or `enforce`.
    pub csp_mode: String,
    /// The directive rows.
    #[serde(default)]
    pub csp: Vec<CspDirective>,
    /// `max_age_seconds: null` turns HSTS off.
    #[serde(default)]
    pub hsts_max_age_seconds: Option<i64>,
    /// Whether the HSTS header carries `includeSubDomains`.
    #[serde(default)]
    pub hsts_include_subdomains: bool,
    /// Whether the HSTS header carries `preload`.
    #[serde(default)]
    pub hsts_preload: bool,
    /// Whether `nosniff` is sent.
    #[serde(default)]
    pub content_type_options: bool,
    /// The referrer policy, or `None`/`""` to send none.
    #[serde(default)]
    pub referrer_policy: Option<String>,
    /// Permissions-policy entries.
    #[serde(default)]
    pub permissions_policy: Vec<String>,
    /// The document the form was opened with. `None` on a first save.
    #[serde(default)]
    pub expected_document: Option<serde_json::Value>,
}

/// The saved policy, plus the history head so the screen can show "changed 2 minutes ago".
#[derive(Debug, Serialize)]
pub struct SaveHeadersResponse {
    /// The stored policy as it now reads.
    #[serde(flatten)]
    pub headers: HeadersBody,
    /// The new history row's id, so the panel can highlight what it just wrote.
    pub change_id: Option<i64>,
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /security/headers` — the stored policy and what it renders to.
pub async fn get(
    State(state): State<AppState>,
    session: CurrentSession,
) -> Result<Json<HeadersBody>, ApiError> {
    let _ = session;
    let stored = load_headers(state.db().pool()).await.map_err(map_store)?;
    let saved = !stored.document.is_null();
    Ok(Json(build_body(
        &stored.document,
        saved,
        stored.updated_by,
        Some(stored.updated_at),
    )))
}

/// `PUT /security/headers` — validate, store and record who changed it.
pub async fn put(
    State(state): State<AppState>,
    session: CurrentSession,
    body: axum::Json<SaveHeadersBody>,
) -> Result<Response, ApiError> {
    let body = body.0;
    let current = load_headers(state.db().pool()).await.map_err(map_store)?;

    // Validate first. A policy that cannot be applied is refused before anything is written,
    // so a bad edit never reaches the response headers it was meant to shape.
    let mode = CspMode::parse(&body.csp_mode).map_err(map_store)?;
    let policy = HeaderPolicy::new(
        mode,
        body.csp,
        HstsPolicy {
            max_age_seconds: body.hsts_max_age_seconds,
            include_subdomains: body.hsts_include_subdomains,
            preload: body.hsts_preload,
        },
        body.content_type_options,
        body.referrer_policy,
        body.permissions_policy,
    )
    .map_err(map_store)?;

    let document = serde_json::to_value(&policy).map_err(|error| {
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("the header policy could not be serialised: {error}"),
        )
    })?;

    let expected = body
        .expected_document
        .clone()
        .unwrap_or(current.document.clone());
    let saved = save_headers(
        state.db().pool(),
        &document,
        Some(&expected),
        session.user.id,
        policy.csp_mode.as_str(),
    )
    .await
    .map_err(map_store)?;

    // The event is what a webhook subscriber sees (`security.headers.updated`, REQ-012). It
    // carries the directive *names* and the mode — never the values, because a CSP value is
    // still configuration somebody considers sensitive and an event travels further than the
    // panel does.
    //
    // A failure here is logged, not surfaced: the policy has already been stored and the audit
    // entry below is the authoritative record, so answering 500 would tell the operator their
    // edit was lost when it is in the database and on the next response.
    if let Err(error) = bus::emit(
        state.db().pool(),
        NewEvent::new("security.headers.updated")
            .organization(session.user.organization_id)
            .actor(session.user.id)
            .payload(json!({
                "csp_mode": policy.csp_mode.as_str(),
                "directives": policy.csp.iter().map(|row| row.directive.clone()).collect::<Vec<_>>(),
            })),
    )
    .await
    {
        tracing::warn!(error = %error, "the header policy was saved but the event was not recorded");
    }

    record_audit(
        state.db().pool(),
        NewAuditEntry::by_user(session.user.id, "security.headers.updated")
            .organization(session.user.organization_id)
            .target("security_settings", "1")
            .metadata(json!({
                "csp_mode": policy.csp_mode.as_str(),
                "directive_count": policy.csp.len(),
            })),
    )
    .await
    .map_err(|error| {
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("the header policy was saved but could not be audited: {error}"),
        )
    })?;

    // The stored policy now applies to the next response, not at the next restart. A failure
    // here is logged and not surfaced: the write has already committed, and answering 500 would
    // tell the operator their edit was lost when it is in the database.
    if !crate::headers_middleware::reload_from_store(&state).await {
        tracing::info!(
            "the header policy was saved; the running process picks it up from the store"
        );
    }

    let (change_id, _) = omnion_security::header_history(state.db().pool(), 1)
        .await
        .map_err(map_store)?
        .into_iter()
        .next()
        .map_or((None, None), |change| {
            (Some(change.id), Some(change.changed_at))
        });

    let headers = build_body(
        &saved.document,
        true,
        saved.updated_by,
        Some(saved.updated_at),
    );
    Ok(Json(SaveHeadersResponse { headers, change_id }).into_response())
}

/// Assemble the response body from a stored document.
///
/// The document is read through [`HeaderPolicy::from_json`] rather than trusted as a struct,
/// because a row written by an older build (or by hand, in an emergency) may not be one this
/// binary understands — and an unreadable policy must render as the baseline, visibly, not as
/// nothing.
fn build_body(
    document: &serde_json::Value,
    saved: bool,
    updated_by: Option<Uuid>,
    updated_at: Option<OffsetDateTime>,
) -> HeadersBody {
    let policy = HeaderPolicy::from_json(Some(document));
    // Rendered BEFORE the fields are moved out: the preview is the same rendering the
    // middleware applies, and reading it from a half-moved policy is how the two drift apart.
    let rendered = policy
        .render()
        .into_iter()
        .map(|line| HeaderLineBody {
            name: line.name,
            value: line.value,
        })
        .collect();
    HeadersBody {
        csp_mode: policy.csp_mode.as_str().to_owned(),
        csp: policy.csp,
        hsts: policy.hsts,
        content_type_options: policy.content_type_options,
        referrer_policy: policy.referrer_policy,
        permissions_policy: policy.permissions_policy,
        rendered,
        saved,
        updated_by,
        updated_at: updated_at.map(|at| at.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved_policy() -> serde_json::Value {
        serde_json::to_value(HeaderPolicy::default()).expect("the default policy serialises")
    }

    #[test]
    fn an_unsaved_row_renders_the_baseline_and_says_so() {
        let body = build_body(&serde_json::Value::Null, false, None, None);
        assert!(!body.saved, "an operator has not chosen a policy yet");
        assert!(
            !body.rendered.is_empty(),
            "the baseline still sends headers — 'unsaved' must not mean 'none'"
        );
        assert_eq!(body.updated_at, None);
    }

    #[test]
    fn the_rendered_column_is_the_render_not_a_summary() {
        let body = build_body(&saved_policy(), true, None, None);
        let names: Vec<&str> = body
            .rendered
            .iter()
            .map(|line| line.name.as_str())
            .collect();
        assert!(names.contains(&"Content-Security-Policy-Report-Only"));
        assert!(names.contains(&"Strict-Transport-Security"));
        // And it carries a value, not just a name — a preview with no value teaches nothing.
        assert!(body.rendered.iter().all(|line| line.value.is_some()));
    }

    #[test]
    fn a_header_that_is_off_is_a_row_with_no_value_rather_than_a_missing_row() {
        let mut policy = HeaderPolicy::default();
        policy.hsts.max_age_seconds = None;
        let document = serde_json::to_value(policy).expect("serialise");
        let body = build_body(&document, true, None, None);
        let hsts = body
            .rendered
            .iter()
            .find(|line| line.name == "Strict-Transport-Security")
            .expect("HSTS is always listed");
        assert_eq!(hsts.value, None, "an off header must be visible as off");
    }

    #[test]
    fn a_document_this_build_does_not_understand_renders_as_the_baseline() {
        let body = build_body(
            &serde_json::json!({ "csp": "not-an-array" }),
            true,
            None,
            None,
        );
        assert_eq!(
            body.csp_mode, "report_only",
            "an unreadable document falls back rather than sending nothing"
        );
        assert!(!body.rendered.is_empty());
    }
}
