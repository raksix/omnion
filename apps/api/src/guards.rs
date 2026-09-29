//! Route guards.
//!
//! `require(permission)` wraps a route so only sessions that carry the permission
//! (docs/07-IAM.md §2) reach the handler. The guard resolves the session once and shares it
//! with the handler through the request extensions, so `CurrentSession` costs no extra round
//! trip.
//!
//! Authorisation runs in the caller's own scope: their primary organization, or the platform
//! level for accounts that belong to none. Endpoints that act on another scope check it in the
//! handler, where the target is known.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use omnion_identity::User;
use omnion_permissions::model::{ResourceContext, Subject};
use omnion_permissions::{Decision, Scope, authorize, authorize_subject, simulate};
use serde_json::json;
use tower::{Layer, Service};
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

/// The scope a plain request is authorised in.
#[must_use]
pub fn scope_of(user: &User) -> Scope {
    match user.organization_id {
        Some(organization_id) => Scope::Organization { organization_id },
        None => Scope::Global,
    }
}

/// A request that authenticated with a machine key instead of a session.
#[derive(Debug, Clone)]
pub struct MachinePrincipal {
    /// The identity the key belongs to.
    pub account: omnion_permissions::model::Subject,
    /// Where the identity works (its organization).
    pub organization_id: Uuid,
    /// The key that was presented.
    pub key_id: Uuid,
}

/// Whether a guard accepts only sessions, or also machine keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardKind {
    /// A signed-in session (the cookie).
    Session,
    /// A session or a service-account key presented as `Authorization: Bearer`.
    SessionOrMachine,
}

/// Layer rejecting requests whose caller does not carry `permission`.
#[derive(Clone)]
pub struct RequirePermission {
    state: AppState,
    permission: &'static str,
    kind: GuardKind,
    /// A second permission that also opens the route, when the layer is an "any of" one.
    ///
    /// A `&'static [&'static str]` rather than a `Vec` because the set is fixed at wiring time:
    /// there is nothing to allocate and nothing a caller can change after the fact.
    alternatives: &'static [&'static str],
}

impl RequirePermission {
    /// Build the layer for one permission.
    #[must_use]
    pub fn new(state: AppState, permission: &'static str) -> Self {
        Self {
            state,
            permission,
            kind: GuardKind::Session,
            alternatives: &[],
        }
    }

    /// Build a layer that opens for **any one** of the named permissions.
    ///
    /// Its own constructor because "or" cannot be spelled by picking a primary and hoping: the
    /// check has to try each and refuse only when all of them say no, and a caller passing the
    /// first name as `permission` with the rest as alternatives would have to know that the
    /// primary is tried first and why — exactly the kind of knowledge a route author should not
    /// need. The refusal names **all** of them, so somebody who got a 403 sees the whole set
    /// they were measured against rather than the first name that happened to be tried.
    #[must_use]
    pub fn new_any(state: AppState, permissions: &'static [&'static str]) -> Self {
        let (first, rest) = permissions.split_first().expect("a guard needs one permission");
        Self {
            state,
            permission: first,
            kind: GuardKind::Session,
            alternatives: rest,
        }
    }

    /// Build a layer that also accepts a machine key over `Authorization: Bearer`.
    #[must_use]
    pub fn new_with_machine(state: AppState, permission: &'static str) -> Self {
        Self {
            state,
            permission,
            kind: GuardKind::SessionOrMachine,
            alternatives: &[],
        }
    }
}

/// Guard a route with `permission` (docs/07-IAM.md §2).
#[must_use]
pub fn require(state: &AppState, permission: &'static str) -> RequirePermission {
    RequirePermission::new(state.clone(), permission)
}

/// Guard a route with `permission`, accepting a service-account key as `Bearer` too.
#[must_use]
pub fn require_or_machine(state: &AppState, permission: &'static str) -> RequirePermission {
    RequirePermission::new_with_machine(state.clone(), permission)
}

/// Guard a route with **any one** of `permissions`.
///
/// The only route in the platform that uses it is the sales desk's global search
/// (docs/requests/REQ-052, slice 4b), and the reason it could not simply be two routes is worth
/// recording: the ⌘K palette is on every screen, so a person whose job is deliveries and not
/// quoting must still find their order by typing a customer's name. Requiring both keys would make
/// the search silently absent for half the sales desk — a feature that vanishes with no message,
/// which is worse than a feature that is not there.
#[must_use]
pub fn require_any(state: &AppState, permissions: &'static [&'static str]) -> RequirePermission {
    RequirePermission::new_any(state.clone(), permissions)
}

impl<S> Layer<S> for RequirePermission {
    type Service = RequirePermissionService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequirePermissionService {
            inner,
            state: self.state.clone(),
            permission: self.permission,
            kind: self.kind,
            alternatives: self.alternatives,
        }
    }
}

/// Service produced by [`RequirePermission`].
#[derive(Clone)]
pub struct RequirePermissionService<S> {
    inner: S,
    state: AppState,
    permission: &'static str,
    kind: GuardKind,
    alternatives: &'static [&'static str],
}

impl<S> Service<Request<Body>> for RequirePermissionService<S>
where
    S: Service<Request<Body>, Response = Response<Body>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: Request<Body>) -> Self::Future {
        let state = self.state.clone();
        let permission = self.permission;
        let kind = self.kind;
        let alternatives = self.alternatives;
        // `Request<Body>` is not `Sync`, so the guard works on a copy of the headers and hands
        // the whole request to the handler afterwards.
        let headers = request.headers().clone();
        // `Service::call` takes `&mut self`; the freshly cloned inner service is the one that
        // runs, so the guard stays usable for the next request.
        let mut inner = self.inner.clone();

        Box::pin(async move {
            // An "any of" layer tries **every** name — the primary first, then the
            // alternatives — and keeps the first caller it gets.
            //
            // Both halves of that sentence are load-bearing, and the first version of this
            // got the first half wrong in the quietest possible way: it looped over
            // `alternatives` alone, and `alternatives` holds the *second* name onwards because
            // `new_any` peeled the first one off to be `permission`. So the layer that existed
            // precisely so a quotes-only reader could search was the one case it refused: the
            // quotes-only account holds the primary, the primary was never tried, and the walk
            // caught it as a 403 naming both keys. The refusal held back until the last name has
            // been tried too, or a signed-in holder of the second key would be told they are not
            // signed in.
            if !alternatives.is_empty() {
                let mut last: Option<ApiError> = None;
                for name in std::iter::once(permission).chain(alternatives.iter().copied()) {
                    match check_kind(&state, &headers, name, kind).await {
                        Ok(caller) => {
                            insert_caller(&mut request, caller);
                            return inner.call(request).await;
                        }
                        Err(error) => last = Some(error),
                    }
                }
                let Some(error) = last else {
                    return inner.call(request).await;
                };
                // The refusal names the whole set, not the name that happened to be tried last.
                return Ok(explain_any_denial(error, permission, alternatives).into_response());
            }
            match check_kind(&state, &headers, permission, kind).await {
                Ok(Caller::Session(session)) => {
                    request.extensions_mut().insert(*session);
                    inner.call(request).await
                }
                Ok(Caller::Machine(machine)) => {
                    request.extensions_mut().insert(machine);
                    inner.call(request).await
                }
                Err(error) => Ok(error.into_response()),
            }
        })
    }
}

/// Put a resolved caller into the request's extensions, whichever kind it is.
fn insert_caller(request: &mut Request<Body>, caller: Caller) {
    match caller {
        Caller::Session(session) => {
            request.extensions_mut().insert(*session);
        }
        Caller::Machine(machine) => {
            request.extensions_mut().insert(machine);
        }
    }
}

/// A refusal from an "any of" guard, naming every permission that was tried.
///
/// The single-permission refusal explains one key; this one has to explain the set, because a
/// person who ran the simulator against their own role and still got a 403 needs to see the whole
/// list they were measured against. A `401` passes through untouched — there is no permission set
/// to explain to somebody who is not signed in at all.
fn explain_any_denial(
    error: ApiError,
    first: &'static str,
    rest: &'static [&'static str],
) -> ApiError {
    if error.status() != StatusCode::FORBIDDEN {
        return error;
    }
    let names: Vec<&str> = std::iter::once(first).chain(rest.iter().copied()).collect();
    ApiError::forbidden(
        "permission_denied",
        format!("this needs one of: {}", names.join(", ")),
    )
}

/// Who called a guarded route.
///
/// The session is boxed: it carries the account and its methods, and a `Caller` is built once
/// per request and immediately taken apart, so the enum stays small without an extra allocation
/// per request mattering.
#[derive(Debug, Clone)]
pub enum Caller {
    /// A signed-in session.
    Session(Box<CurrentSession>),
    /// A service account that presented a key.
    Machine(MachinePrincipal),
}

/// Resolve the session and require one permission in the caller's own scope.
///
/// `401` when there is no usable session, `403 permission_denied` when the session simply does
/// not carry the permission.
pub async fn check(
    state: &AppState,
    headers: &HeaderMap,
    permission: &str,
) -> Result<CurrentSession, ApiError> {
    match check_kind(state, headers, permission, GuardKind::Session).await? {
        Caller::Session(session) => Ok(*session),
        // Unreachable: a session-only guard never resolves a machine.
        Caller::Machine(_) => Err(ApiError::unauthorized(
            "unauthenticated",
            "sign in to continue",
        )),
    }
}

/// Resolve the caller (session, or a machine key when the guard accepts one) and require
/// `permission` of them.
pub async fn check_kind(
    state: &AppState,
    headers: &HeaderMap,
    permission: &str,
    kind: GuardKind,
) -> Result<Caller, ApiError> {
    // A session cookie wins: a signed-in person is never mistaken for a machine.
    if crate::cookies::session_token(headers).is_some() {
        let session = CurrentSession::resolve(state, headers).await?;
        let scope = scope_of(&session.user);
        let context = ResourceContext::from_scope(scope.clone());
        return match authorize(state.db().pool(), session.user.id, scope, permission).await? {
            Decision::Allowed(_) => Ok(Caller::Session(Box::new(session))),
            Decision::Denied { reason, source } => {
                tracing::debug!(
                    permission,
                    user_id = %session.user.id,
                    ?reason,
                    role = source.map(|grant| grant.role_key),
                    "permission denied"
                );
                Err(
                    explain_denial(state, Subject::User(session.user.id), &context, permission)
                        .await,
                )
            }
        };
    }

    // No cookie: a machine key may authenticate, when the route accepts one.
    if kind == GuardKind::SessionOrMachine {
        if let Some(bearer) = bearer_token(headers) {
            let Some(machine) =
                omnion_permissions::service_accounts::authenticate(state.db().pool(), &bearer)
                    .await?
            else {
                return Err(ApiError::unauthorized(
                    "invalid_machine_key",
                    "this machine key is unknown, revoked or does not match its secret",
                ));
            };

            let subject = omnion_permissions::model::Subject::ServiceAccount(machine.account.id);
            let context = omnion_permissions::model::ResourceContext {
                organization_id: Some(machine.account.organization_id),
                ..omnion_permissions::model::ResourceContext::default()
            };

            return match authorize_subject(state.db().pool(), subject, &context, permission).await?
            {
                Decision::Allowed(_) => Ok(Caller::Machine(MachinePrincipal {
                    account: subject,
                    organization_id: machine.account.organization_id,
                    key_id: machine.key.id,
                })),
                Decision::Denied { reason, source } => {
                    tracing::debug!(
                        permission,
                        service_account = %machine.account.id,
                        ?reason,
                        role = source.map(|grant| grant.role_key),
                        "machine permission denied"
                    );
                    Err(explain_denial(state, subject, &context, permission).await)
                }
            };
        }
    }

    Err(ApiError::unauthorized(
        "unauthenticated",
        "sign in to continue",
    ))
}

/// The refusal both callers get.
///
/// The body names the decision source (docs/07-IAM.md §18): the permission, why the answer was
/// no, and — when a role refused it — which role, through which binding, in which scope. The
/// verdict comes from the same resolution the simulator shows, so a 403 and the simulator can
/// never disagree.
async fn explain_denial(
    state: &AppState,
    subject: Subject,
    context: &ResourceContext,
    permission: &str,
) -> ApiError {
    let refusal = ApiError::forbidden(
        "permission_denied",
        format!("this action requires the \"{permission}\" permission"),
    );
    let Ok(report) = simulate::simulate(state.db().pool(), subject, permission, context).await
    else {
        return refusal;
    };

    refusal.with_details(json!({
        "permission": permission,
        "reason": report.reason,
        "source": report.source.map(|source| json!({
            "role_key": source.role_key,
            "role_name": source.role_name,
            "via": source.via,
            "policy": source.policy.map(|policy| json!({
                "policy_id": policy.policy_id,
                "policy_name": policy.policy_name,
                "priority": policy.priority,
            })),
        })),
        "context": json!({
            "organization_id": context.organization_id,
            "site_id": context.site_id,
            "department": context.department,
            "module": context.module,
            "path": context.path,
        }),
        "considered": report.considered,
        "counted": report.counted,
        "note": report.note,
    }))
}

/// The `Bearer` token of a request, when it presents one.
#[must_use]
pub fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let token = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?;
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    Some(token.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;
    use uuid::Uuid;

    fn user(organization_id: Option<Uuid>) -> User {
        User {
            id: Uuid::nil(),
            organization_id,
            email: "ada@example.com".to_owned(),
            display_name: "Ada".to_owned(),
            status: "active".to_owned(),
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn accounts_are_authorised_in_their_own_scope() {
        let organization_id = Uuid::new_v4();
        assert_eq!(
            scope_of(&user(Some(organization_id))),
            Scope::Organization { organization_id }
        );
        assert_eq!(scope_of(&user(None)), Scope::Global);
    }
}
