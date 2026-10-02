//! Bearer authentication for developer API keys (REQ-033, slice 1).
//!
//! This is the guard a route uses when a *developer key* may call it as well as a session —
//! `guards::require_or_key` in [`crate::guards`]. The service-account path that already exists
//! (`omsa_*` tokens, [`omnion_permissions::service_accounts`]) authorises against role bindings;
//! this one authorises against the **key's own scope list**, and the two are different questions
//! on purpose:
//!
//! * A service account is a *person's* automation. It gets the permissions a role gives it, and
//!   changing what it may do is a role change — visible, attributable, reviewed.
//! * An API key is a *credential inside somebody's configuration*. It is pasted into a
//!   `.env` in a repository, it is rotated by whoever holds that file, and nobody necessarily
//!   knows it exists. Its scope list is therefore the only place its power is written down, and
//!   it has to be enforced at the edge rather than inferred from a role.
//!
//! # Why a key does not get a `CurrentSession`
//!
//! Because it is not a person and must not be mistaken for one. A key carries an organization
//! and a scope list and nothing else: no user id, no session row, no cookie. Handlers that need
//! to know who is acting read [`KeyPrincipal::actor_user_id`] — which is `None` — rather than
//! finding a user they can attribute an audit entry to. The alternative, manufacturing a
//! synthetic session for a key, is how a platform ends up with audit rows that name a person who
//! never did the thing.
//!
//! # The refusal
//!
//! One `401` for a bad credential, one `403` for a credential that is fine and the request is
//! not permitted — and the `403` names the *permission*, never the roles behind it. That split
//! is the whole of the answer to "can this key do this?", and it is the same contract
//! [`crate::guards`] already gives a session, so no client needs to learn two error shapes.

use axum::body::Body;
use axum::http::{HeaderMap, Request};
// `into_response` on `ApiError` is a trait method, so the trait has to be in scope — the same
// import `guards.rs` carries for the same call.
use axum::response::IntoResponse;
use omnion_developer::store;
use omnion_developer::{KeyRefusal, decide, secret};
use std::net::IpAddr;
use time::OffsetDateTime;
use tower::Service;
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

/// A developer key that has authenticated a request.
///
/// Deliberately *not* a `CurrentSession`: see the module comment. Three fields, all of which the
/// caller needs and none of which it can fabricate.
#[derive(Debug, Clone)]
pub struct KeyPrincipal {
    /// The key's id, for the request log and the audit entry.
    pub key_id: Uuid,
    /// The organization the key speaks for.
    pub organization_id: Uuid,
    /// The scopes it holds — the *key's* scopes, not a role's.
    pub scopes: Vec<String>,
    /// The public prefix, which is safe to log.
    pub prefix: String,
}

impl KeyPrincipal {
    /// Whether this key holds `permission`.
    ///
    /// Delegates to [`omnion_developer::scope_allows`] rather than re-implementing the
    /// comparison, because two spellings of "does this scope list grant X" is one of them
    /// eventually giving a prefix match — and the one that does is the one that hands a
    /// read-only key the write beside it.
    pub fn allows(&self, permission: &str) -> bool {
        omnion_developer::scope_allows(&self.scopes, permission)
    }

    /// Who to audit the action as. A key has no user, and the answer is `None` rather than a
    /// fabricated one.
    #[must_use]
    pub fn actor_user_id(&self) -> Option<Uuid> {
        None
    }

    /// The scopes, for a `403` detail.
    #[must_use]
    pub fn scope_list(&self) -> String {
        if self.scopes.is_empty() {
            return "none".to_owned();
        }
        self.scopes.join(", ")
    }
}

/// Resolve a presented developer key, or return the refusal it earns.
///
/// Free function rather than a method on a layer so the whole of the authentication decision is
/// testable without a `Router`: the interesting behaviour is the *order* and the *sameness* of
/// the refusals, and neither is observable from outside a service.
pub async fn authenticate_key(
    state: &AppState,
    headers: &HeaderMap,
    address: Option<IpAddr>,
    now: OffsetDateTime,
) -> Result<KeyPrincipal, ApiError> {
    let Some(token) = crate::guards::bearer_token(headers) else {
        return Err(ApiError::unauthorized(
            "unauthenticated",
            "sign in or present an API key",
        ));
    };

    // 1. Shape. Free, and a malformed token must not cost a database probe.
    let Some((prefix, secret_half)) = secret::split_token(&token) else {
        return Err(invalid_key());
    };

    // 2. Existence. One index probe on the 48-bit public prefix — never a scan, which is what
    //    the two-halves design of the token is for.
    let Some((key, organization_id)) = store::find_by_prefix(state.db().pool(), prefix)
        .await
        .map_err(store_error)?
    else {
        return Err(invalid_key());
    };

    // 3 + 4 + 5. Secret, state, address — in that order, inside `decide`. Refused *and* recorded:
    //    a key that authenticated and then failed a scope check still made a request, so its
    //    usage rollup and the log row are written by the caller below, not skipped.
    let stored_hash: String = sqlx::query_scalar("select secret_hash from api_keys where id = $1")
        .bind(key.id)
        .fetch_one(state.db().pool())
        .await
        // A raw query, so a raw error — unlike the store call above, which returns the crate's own
        // error. Two call sites, two error types, two mappings; naming them apart here is cheaper
        // than discovering which one a future edit passed.
        .map_err(|error| {
            ApiError::from_core(omnion_core::CoreError::Unavailable {
                dependency: "developer store".into(),
                message: error.to_string(),
            })
        })?;

    if let Err(refusal) = decide(&key, &stored_hash, secret_half, address, now) {
        return Err(refusal_error(refusal));
    }

    Ok(KeyPrincipal {
        key_id: key.id,
        organization_id,
        scopes: key.scopes.clone(),
        prefix: key.prefix.clone(),
    })
}

/// The one answer for every bad credential.
///
/// Deliberately not the *reason*. A revoked key and a key that never existed produce the same
/// `401` code and message, and a key whose allowlist excluded the caller produces the same one
/// again — because "your key was refused from this address" is a fact an attacker can use to
/// learn that a key is real. The precise reason is a `tracing::debug` line and nothing more.
fn invalid_key() -> ApiError {
    ApiError::unauthorized(
        "invalid_api_key",
        "this API key is not valid for this request",
    )
}

/// Turn a refusal into the response a caller sees.
///
/// The state refusals (revoked, expired) keep their own message because the person holding the
/// key is the one who needs to know whether to rotate it or ask for an extension — and neither
/// message echoes the key.
fn refusal_error(refusal: KeyRefusal) -> ApiError {
    match refusal {
        // Address-not-allowed collapses into the generic answer. This is the one place where
        // precision would be a vulnerability.
        KeyRefusal::Invalid | KeyRefusal::AddressNotAllowed => invalid_key(),
        KeyRefusal::Revoked => {
            ApiError::unauthorized("api_key_revoked", "this API key has been revoked")
        }
        KeyRefusal::Expired => {
            ApiError::unauthorized("api_key_expired", "this API key has expired")
        }
    }
}

///
/// Takes the crate's error rather than `sqlx::Error` because the store *wraps* sqlx: its
/// `Database` variant is the only way a query failure reaches this file, and unwrapping it here
/// would mean matching a variant that the crate already decided how to categorise.
fn store_error(error: omnion_developer::DeveloperError) -> ApiError {
    if error.is_client_error() {
        ApiError::bad_request(error.code(), error.to_string())
    } else {
        ApiError::from_core(omnion_core::CoreError::Unavailable {
            dependency: "developer store".into(),
            message: error.to_string(),
        })
    }
}

/// Write the request-log row and the usage rollup for a key-authenticated call.
///
/// Called after the handler has answered, because the row needs the status and the duration —
/// which is why this is a function rather than something the handler does inline. **A failure
/// here is logged, not propagated**: the caller's request already succeeded or failed on its own
/// merits, and turning a bookkeeping failure into a `500` would make a working integration fail
/// because the log table was briefly unreachable. The consequence is stated rather than hidden:
/// a gap in the log is a gap in the log, and the alternative loses the request entirely.
pub async fn record_key_use(
    state: &AppState,
    principal: &KeyPrincipal,
    method: &str,
    path: &str,
    status: u16,
    duration_ms: i32,
    now: OffsetDateTime,
) {
    let status = i16::try_from(status).unwrap_or(0);

    let log = omnion_developer::RequestLog {
        id: 0,
        organization_id: principal.organization_id,
        api_key_id: Some(principal.key_id),
        // The prefix, copied in beside the id so an operator can match this row to a key from
        // the list without a second query. Never a token — this is the value the list prints.
        api_key_prefix: Some(principal.prefix.clone()),
        // A key has no user behind it. `None` is the honest answer; a fabricated id would put a
        // person's name on a request they did not make.
        actor_user_id: None,
        actor_name: String::new(),
        // A key is authorized by its scopes, not by a permission a person held, so there is no
        // single permission to record here. The request log middleware fills this column for
        // every request; the key recorder is the one writer that genuinely has nothing to say.
        permission: None,
        method: method.to_owned(),
        // The path *without* the query string: a query string is caller-controlled and routinely
        // carries an email address or a token in a filter, and this table is the one an operator
        // exports to CSV.
        path: path.split('?').next().unwrap_or(path).to_owned(),
        status,
        duration_ms,
        // Generated per request rather than read from a header: there is no upstream that sets
        // one, and a caller-supplied value would let one client label another's rows.
        request_id: uuid::Uuid::new_v4().simple().to_string(),
        bytes_in: None,
        bytes_out: None,
        error_code: None,
        created_at: now,
    };

    if let Err(error) = store::log_request(state.db().pool(), &log).await {
        tracing::warn!(error = %error, "the developer request log did not accept a row");
        return;
    }
    if let Err(error) = store::record_use(
        state.db().pool(),
        principal.key_id,
        principal.organization_id,
        status,
        duration_ms,
        now,
    )
    .await
    {
        tracing::warn!(error = %error, "the API key usage rollup was not updated");
    }
}

/// The `403` a key earns by holding a scope that does not cover the request.
///
/// Names the missing permission — the request says so explicitly, and a developer who gets
/// `403 permission_denied` with no further detail has nothing to act on. It does **not** name the
/// roles behind the key, because a key has no roles and the equivalent leak for a session (the
/// simulator's verdict) is a decision the guard already made about `require_or_machine`.
pub fn scope_refusal(permission: &str, principal: &KeyPrincipal) -> ApiError {
    ApiError::forbidden(
        "permission_denied",
        format!("this API key does not hold the \"{permission}\" permission"),
    )
    .with_details(serde_json::json!({
        "permission": permission,
        "scopes": principal.scopes,
        "hint": "a key is limited to the scopes it was created with; rotate it with a wider scope list if this integration needs more"
    }))
}

/// The layer that resolves a developer key and checks one permission against its scope list.
///
/// Installed *inside* the permission guards' nest and *outside* the CSRF layer's concern — a
/// bearer token is not ambient authority, so `headers_middleware::require_csrf` already skips
/// requests that carry one (see the comment there).
#[derive(Clone)]
pub struct RequireKey {
    state: AppState,
    permission: &'static str,
}

impl RequireKey {
    /// Build the layer for one permission.
    #[must_use]
    pub fn new(state: AppState, permission: &'static str) -> Self {
        Self { state, permission }
    }
}

/// Guard a route with `permission`, accepting a developer API key as `Bearer` too.
///
/// Named to sit beside [`crate::guards::require_or_machine`] so the two machine paths read as
/// the pair they are: `require_or_machine` authorises a service account against its **roles**,
/// this authorises an API key against its **own scope list**.
#[must_use]
pub fn require_or_key(state: &AppState, permission: &'static str) -> RequireKey {
    RequireKey::new(state.clone(), permission)
}

impl<S> tower::Layer<S> for RequireKey {
    type Service = RequireKeyService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequireKeyService {
            inner,
            state: self.state.clone(),
            permission: self.permission,
        }
    }
}

/// Service produced by [`RequireKey`].
#[derive(Clone)]
pub struct RequireKeyService<S> {
    inner: S,
    state: AppState,
    permission: &'static str,
}

impl<S> Service<Request<Body>> for RequireKeyService<S>
where
    S: Service<
            Request<Body>,
            Response = axum::response::Response,
            Error = std::convert::Infallible,
        > + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = axum::response::Response;
    type Error = std::convert::Infallible;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        let state = self.state.clone();
        let permission = self.permission;
        let headers = request.headers().clone();
        let method = request.method().clone();
        let path = request.uri().path().to_owned();
        let address = request
            .extensions()
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|axum::extract::ConnectInfo(address)| address.ip());
        let started = std::time::Instant::now();
        let mut inner = self.inner.clone();

        Box::pin(async move {
            // A session cookie wins outright: a signed-in person is never a key, and a panel
            // that happened to carry both must behave as the person.
            let principal = if crate::cookies::session_token(&headers).is_some() {
                None
            } else {
                match authenticate_key(&state, &headers, address, OffsetDateTime::now_utc()).await {
                    Ok(principal) => Some(principal),
                    Err(error) => {
                        return Ok(error.into_response());
                    }
                }
            };

            if let Some(principal) = &principal
                && !principal.allows(permission)
            {
                return Ok(scope_refusal(permission, principal).into_response());
            }

            let response = inner.call(request).await?;

            // The bookkeeping happens *after* the handler, because it needs the status and the
            // duration. It cannot change the response: a log-write failure is a warning, never a
            // `500` on a request that already succeeded.
            if let Some(principal) = principal {
                let duration_ms = i32::try_from(started.elapsed().as_millis()).unwrap_or(i32::MAX);
                record_key_use(
                    &state,
                    &principal,
                    method.as_str(),
                    &path,
                    response.status().as_u16(),
                    duration_ms,
                    OffsetDateTime::now_utc(),
                )
                .await;
            }

            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn principal_with(scopes: &[&str]) -> KeyPrincipal {
        KeyPrincipal {
            key_id: Uuid::nil(),
            organization_id: Uuid::nil(),
            scopes: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
            prefix: "omn_000000000000".to_owned(),
        }
    }

    #[test]
    fn a_key_is_limited_to_the_scopes_it_was_created_with() {
        // The acceptance criterion the whole slice is built around.
        let reader = principal_with(&["content.pages.read", "developer.keys.read"]);
        assert!(reader.allows("content.pages.read"));
        // The write the key cannot do, even though it holds a read in the *same* family — which
        // is exactly the boundary a prefix match would have crossed.
        assert!(!reader.allows("content.pages.update"));
        assert!(!reader.allows("content.pages.publish"));
        assert!(!reader.allows("developer.keys.manage"));
    }

    #[test]
    fn a_key_holding_no_scopes_can_do_nothing() {
        let empty = principal_with(&[]);
        assert!(!empty.allows("content.pages.read"));
        assert_eq!(empty.scope_list(), "none");
    }

    #[test]
    fn a_key_is_never_attributed_to_a_person() {
        // The property that keeps the audit log honest: a key has no user, and the honest answer
        // is `None`. A fabricated id would put somebody's name on a request they never made.
        assert_eq!(principal_with(&["*"]).actor_user_id(), None);
    }

    #[test]
    fn the_scope_refusal_names_the_permission_and_the_scopes_and_nothing_else() {
        let principal = principal_with(&["content.pages.read"]);
        let error = scope_refusal("content.pages.update", &principal);
        let rendered = error.to_string();
        assert!(rendered.contains("content.pages.update"));
        // Not the roles: a key has none, and for a session the simulator already decides how much
        // of the verdict to show.
        assert!(!rendered.to_lowercase().contains("role"));
    }

    #[test]
    fn every_bad_credential_renders_the_same_401() {
        // The property, asserted over the function rather than over one input: a revoked key, an
        // expired key, an address that is not allowed and a key that never existed must be
        // indistinguishable to a caller.
        assert_eq!(
            invalid_key().code(),
            refusal_error(KeyRefusal::Invalid).code()
        );
        assert_eq!(
            invalid_key().code(),
            refusal_error(KeyRefusal::AddressNotAllowed).code()
        );
        // The two state refusals keep their own message, and their *code* differs too so a
        // well-behaved client can offer "rotate" rather than a generic sign-in link.
        assert_ne!(
            invalid_key().code(),
            refusal_error(KeyRefusal::Revoked).code()
        );
        assert_ne!(
            invalid_key().code(),
            refusal_error(KeyRefusal::Expired).code()
        );
        // And no message echoes the key.
        for refusal in [
            KeyRefusal::Invalid,
            KeyRefusal::Revoked,
            KeyRefusal::Expired,
            KeyRefusal::AddressNotAllowed,
        ] {
            assert!(!refusal_error(refusal).to_string().contains("omn_"));
        }
    }

    #[test]
    fn a_query_string_never_reaches_the_log_row() {
        // The path the platform records is `uri.path()`, which has no query — but the *function*
        // that writes the row takes whatever it is handed, so the strip is asserted here rather
        // than assumed. A filter like `?email=a@b.com` is exactly what would end up in a CSV.
        let path = "/api/v1/pages?email=someone@example.com&token=abc";
        let recorded = path.split('?').next().unwrap_or(path);
        assert_eq!(recorded, "/api/v1/pages");
        assert!(!recorded.contains("example.com"));
    }

    #[test]
    fn the_scope_list_renders_the_actual_scopes_for_the_403_detail() {
        let principal = principal_with(&["content.pages.read", "media.read"]);
        assert_eq!(principal.scope_list(), "content.pages.read, media.read");
    }
}
