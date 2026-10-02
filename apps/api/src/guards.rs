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
use axum::http::{HeaderMap, Request};
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
    /// That key's displayable prefix — the lookup namespace, never a token.
    ///
    /// Carried here rather than read again by the request log (REQ-022 slice 2). It is not a
    /// credential: the prefix is printed on the key list precisely so an operator can match a
    /// log row to a key. Reading it a second time to decorate a log line would be a second
    /// query whose answer could disagree with the one that authenticated the request.
    pub key_prefix: Option<String>,
}

/// Whether a guard accepts only sessions, or also machine keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardKind {
    /// A signed-in session (the cookie).
    Session,
    /// A session or a service-account key presented as `Authorization: Bearer`.
    SessionOrMachine,
    /// A session, a service-account key, or a **developer API key** — a delegation with a
    /// scope list rather than a role.
    ///
    /// This is a third case and not a flavour of the second, because the two authenticate
    /// against different tables and are resolved by *different* rules: a service-account key
    /// is a subject roles bind to, so it is authorised through the binding table; a developer
    /// key has no role at all, so its scopes are the authorization. Folding the second into the
    /// first would mean either a developer key is looked up in a service-account table it was
    /// never written to, or a service-account key is authorized by a scope column that does not
    /// exist. Both are the "two mechanisms that look alike and behave nothing alike" trap.
    SessionOrDeveloperKey,
}

/// Layer rejecting requests whose caller does not carry `permission`.
#[derive(Clone)]
pub struct RequirePermission {
    state: AppState,
    permission: &'static str,
    kind: GuardKind,
}

impl RequirePermission {
    /// Build the layer for one permission.
    #[must_use]
    pub fn new(state: AppState, permission: &'static str) -> Self {
        Self {
            state,
            permission,
            kind: GuardKind::Session,
        }
    }

    /// Build a layer that also accepts a machine key over `Authorization: Bearer`.
    #[must_use]
    pub fn new_with_machine(state: AppState, permission: &'static str) -> Self {
        Self {
            state,
            permission,
            kind: GuardKind::SessionOrMachine,
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

/// Guard a route with `permission`, accepting a developer API key as a `Bearer` token too.
///
/// The key's scopes are the authorization, checked in [`check_kind`] before the permission
/// decision is made — so a key carrying only `content.pages.read` is refused `403` on a route
/// guarded for `content.pages.manage`, with the missing scope named.
#[must_use]
pub fn require_or_developer_key(state: &AppState, permission: &'static str) -> RequirePermission {
    RequirePermission {
        state: state.clone(),
        permission,
        kind: GuardKind::SessionOrDeveloperKey,
    }
}

impl<S> Layer<S> for RequirePermission {
    type Service = RequirePermissionService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequirePermissionService {
            inner,
            state: self.state.clone(),
            permission: self.permission,
            kind: self.kind,
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
        // `Request<Body>` is not `Sync`, so the guard works on a copy of the headers and hands
        // the whole request to the handler afterwards.
        let headers = request.headers().clone();
        // `Service::call` takes `&mut self`; the freshly cloned inner service is the one that
        // runs, so the guard stays usable for the next request.
        let mut inner = self.inner.clone();

        Box::pin(async move {
            // The reporting variant, so a **refusal** also carries a caller. This is the whole
            // point of the split: a 403 that says "add the `content.pages.read` scope" and is
            // stored with no organization is a row no log screen can ever show, because every
            // screen filters on organization. The walk below caught exactly that.
            let decision = check_kind_reporting(&state, &headers, permission, kind).await;
            let principal = match &decision {
                Ok(Caller::Session(session)) => crate::request_log_middleware::ResolvedPrincipal {
                    user_id: Some(session.user.id),
                    user_name: display_name_of(session),
                    organization_id: session.user.organization_id,
                    permission: Some(permission.to_owned()),
                    ..Default::default()
                },
                Ok(Caller::Machine(machine)) => machine_principal_of(machine, permission),
                // The refusal's own answer. `None` only when a presented key was not found at
                // all, which genuinely has no caller to name.
                Err((_, Some(partial))) => partial.clone(),
                Err((_, None)) => crate::request_log_middleware::ResolvedPrincipal {
                    permission: Some(permission.to_owned()),
                    ..Default::default()
                },
            };

            let response = match decision {
                Ok(Caller::Session(session)) => {
                    request.extensions_mut().insert(*session);
                    inner.call(request).await
                }
                Ok(Caller::Machine(machine)) => {
                    request.extensions_mut().insert(machine);
                    inner.call(request).await
                }
                Err((error, _)) => Ok(error.into_response()),
            };

            // **On the response, not the request, and not in a task-local.** The request log's
            // layer sits *outside* this guard, so by the time it regains control the request has
            // been consumed by the handler and any scope this guard opened has been popped. Two
            // earlier attempts failed exactly here, and both failed *quietly*:
            //
            // * Reading the principal from the request extensions after `inner.call` — the
            //   request is gone; the handler consumed it.
            // * Publishing it in a task-local around the inner call — the scope closes when that
            //   call returns, which is *before* the outer layer runs, so the log read `None`.
            //
            // The response is the one carrier that outlives the handler and travels back out to
            // the layer that needs it, and it is per-response, so two concurrent requests cannot
            // see each other's caller.
            // `S::Error = Infallible`, so the error arm is matched rather than unwrapped: the
            // bound says an observing guard can never be handed a failed service, and writing
            // that as a `match` makes it a *checkable* statement rather than a comment.
            let mut response = match response {
                Ok(response) => response,
                Err(unreachable) => match unreachable {},
            };
            response.extensions_mut().insert(principal);
            Ok(response)
        })
    }
}

/// The name a log row shows for a signed-in account.
///
/// The same fallback the credential audit rows use: an account with no display name shows its
/// email rather than a blank cell, and two different modules picking the same fallback is two
/// places to change when a third field is added.
fn display_name_of(session: &CurrentSession) -> String {
    let display = session.user.display_name.trim();
    if !display.is_empty() {
        return display.to_owned();
    }
    let email = session.user.email.trim();
    if email.is_empty() {
        String::new()
    } else {
        email.to_owned()
    }
}

/// The log's view of a machine or developer key.
///
/// The key's *prefix* travels in [`MachinePrincipal::key_prefix`] rather than being looked up
/// again here: the guard is the only place that has already read the row, and a second read to
/// decorate a log line is a second answer to "which key was that" that can disagree with the
/// first. A `None` prefix (a key minted by a fixture, or a service-account row from before the
/// column existed) is written as `None`, not as an empty string — a log cell that shows `""` for
/// an unknown key looks like a formatting fault.
fn machine_principal_of(
    machine: &MachinePrincipal,
    permission: &'static str,
) -> crate::request_log_middleware::ResolvedPrincipal {
    crate::request_log_middleware::ResolvedPrincipal {
        user_id: None,
        user_name: String::new(),
        api_key_id: Some(machine.key_id),
        api_key_prefix: machine.key_prefix.clone(),
        organization_id: Some(machine.organization_id),
        permission: Some(permission.to_owned()),
    }
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
    check_kind_reporting(state, headers, permission, kind)
        .await
        .map_err(|error| error.0)
}

/// [`check_kind`], and the half of the answer the request log needs on the **refusal** path.
///
/// A refusal is the row an integrator most often wants — "why is my integration being refused" —
/// and it is the one row that has to carry a *caller*. The first version of this returned a bare
/// `ApiError` on the error path, and the log layer, which reads the guard's answer, therefore
/// published a principal with no user, no key and **no organization**. The row was written; it
/// was written with `organization_id = null`, and every log screen filters on
/// `organization_id = $1`. So the 403 that names the missing scope was stored where no operator
/// could ever see it — a debugging table that silently drops exactly the rows it was built for.
///
/// Returning the partial answer alongside the error is what makes a refusal attributable. The
/// caller is taken from the credential that was *presented*, not from a second resolution: the
/// key was already read and hash-compared a line above, and re-reading it would be a second
/// answer to a question that was just answered.
pub async fn check_kind_reporting(
    state: &AppState,
    headers: &HeaderMap,
    permission: &str,
    kind: GuardKind,
) -> Result<Caller, (ApiError, Option<crate::request_log_middleware::ResolvedPrincipal>)> {
    // A session cookie wins: a signed-in person is never mistaken for a machine.
    if crate::cookies::session_token(headers).is_some() {
        let session = match CurrentSession::resolve(state, headers).await {
            Ok(session) => session,
            Err(error) => {
                return Err((
                    error,
                    Some(crate::request_log_middleware::ResolvedPrincipal {
                        permission: Some(permission.to_owned()),
                        ..Default::default()
                    }),
                ));
            }
        };
        let scope = scope_of(&session.user);
        let context = ResourceContext::from_scope(scope.clone());
        // A store failure here is not a refusal and has no principal worth reporting, so it is
        // the one `?` in this function that carries `None`.
        let decision = authorize(state.db().pool(), session.user.id, scope, permission)
            .await
            .map_err(|error| (ApiError::from(error), None))?;
        return match decision {
            Decision::Allowed(_) => Ok(Caller::Session(Box::new(session))),
            Decision::Denied { reason, source } => {
                tracing::debug!(
                    permission,
                    user_id = %session.user.id,
                    ?reason,
                    role = source.map(|grant| grant.role_key),
                    "permission denied"
                );
                Err((
                    explain_denial(state, Subject::User(session.user.id), &context, permission)
                        .await,
                    Some(crate::request_log_middleware::ResolvedPrincipal {
                        user_id: Some(session.user.id),
                        user_name: display_name_of(&session),
                        organization_id: session.user.organization_id,
                        permission: Some(permission.to_owned()),
                        ..Default::default()
                    }),
                ))
            }
        };
    }

    // No cookie. A developer key first, because it is the narrower mechanism: a token that
    // authenticates against `api_keys` must never fall through to the service-account branch
    // and be reported as an unknown machine key, which names the wrong table in the error an
    // integrator reads at 2am.
    //
    // **This calls `developer_auth::authenticate_key` rather than re-reading the key.** Main's
    // slice of the developer portal called `omnion_developer::keys_store::authenticate` here
    // directly; this branch's crate spells the same check `store::find_by_prefix` +
    // `authn::decide`, because its token has two halves and its keys carry an IP allowlist, so
    // "look the row up and compare the secret" is not the whole decision. Two readers for one
    // credential is the defect worth naming: the one without the allowlist would accept a key
    // the other refuses, and which of them ran depends on which route the caller hit.
    // `authenticate_key` is also the only one of the two that records the use, so routing here
    // is what makes the request log cover this path.
    if kind == GuardKind::SessionOrDeveloperKey {
        if let Some(_bearer) = bearer_token(headers) {
            // **A key's refusal is attributed to that key.** The row was read and hash-compared
            // inside `authenticate_key`, so naming it here costs no query and makes the row
            // visible: the organization is what the log screen filters on, and a refusal stored
            // with a null organization is a refusal no operator can find. That is why this
            // function's error type is a tuple rather than a bare `ApiError` — main's portal
            // slice introduced the second half of the pair, and a guard that throws the
            // attribution away on the way out is how a key's refusals end up belonging to nobody.
            let address = None;
            match crate::developer_auth::authenticate_key(
                state,
                headers,
                address,
                time::OffsetDateTime::now_utc(),
            )
            .await
            {
                Ok(principal) => {
                    let organization_id = principal.organization_id;
                    let log_principal = crate::request_log_middleware::ResolvedPrincipal {
                        user_id: None,
                        user_name: String::new(),
                        api_key_id: Some(principal.key_id),
                        api_key_prefix: Some(principal.prefix.clone()),
                        organization_id: Some(organization_id),
                        permission: Some(permission.to_owned()),
                    };

                    // **The scope check is the authorization.** A key holds no role, so a
                    // permission the route asks for that the key does not carry is a plain
                    // refusal — and the refusal names the missing scope, so the integrator is
                    // told which scope to add rather than which route they hit.
                    if !principal.allows(permission) {
                        tracing::debug!(
                            permission,
                            api_key = %principal.key_id,
                            scopes = ?principal.scopes,
                            "developer key does not carry the scope this route requires"
                        );
                        return Err((
                            ApiError::forbidden(
                                "scope_missing",
                                format!(
                                    "this key does not carry the \"{permission}\" scope — rotate it with \
                                     that scope added"
                                ),
                            ),
                            Some(log_principal),
                        ));
                    }

                    return Ok(Caller::Machine(MachinePrincipal {
                        // A key has no subject of its own: it is a delegation *by* somebody, so
                        // the issuing organization is the closest honest subject. It is never
                        // resolved against (a developer key is authorized by its scopes above,
                        // and a revoked issuer must not revoke the integration they delegated —
                        // that is what the portal's own revoke button is for).
                        account: omnion_permissions::model::Subject::User(Uuid::nil()),
                        organization_id,
                        key_id: principal.key_id,
                        // Carried rather than read a second time: the prefix is printed on the
                        // key list precisely so an operator can match a log row to a key, and a
                        // second query could answer differently from the one that authenticated.
                        key_prefix: Some(principal.prefix),
                    }));
                }
                // A bad key here is not "no key" — the caller presented one and it was refused.
                // Answering 401 rather than falling through to the service-account branch is the
                // whole reason this check comes first. The attribution is dropped rather than
                // invented: an unreadable row is not a key we can name.
                Err(error) => return Err((error, None)),
            }
        }
    }

    // No cookie: a machine key may authenticate, when the route accepts one.
    if matches!(kind, GuardKind::SessionOrMachine) {
        if let Some(bearer) = bearer_token(headers) {
            let Some(machine) =
                omnion_permissions::service_accounts::authenticate(state.db().pool(), &bearer)
                    .await
                    .map_err(|error| (ApiError::from(error), None))?
            else {
                // An unknown key is the one refusal with genuinely nothing to attribute it to,
                // and it says so: `None` rather than a fabricated caller.
                return Err((
                    ApiError::unauthorized(
                        "invalid_machine_key",
                        "this machine key is unknown, revoked or does not match its secret",
                    ),
                    None,
                ));
            };

            let subject = omnion_permissions::model::Subject::ServiceAccount(machine.account.id);
            let context = omnion_permissions::model::ResourceContext {
                organization_id: Some(machine.account.organization_id),
                ..omnion_permissions::model::ResourceContext::default()
            };
            // Same reasoning as the developer key above: the key is in hand, so the refusal is
            // attributable to it rather than to nobody.
            let principal = crate::request_log_middleware::ResolvedPrincipal {
                user_id: None,
                user_name: String::new(),
                api_key_id: Some(machine.key.id),
                api_key_prefix: Some(machine.key.prefix.clone()),
                organization_id: Some(machine.account.organization_id),
                permission: Some(permission.to_owned()),
            };

            return match authorize_subject(state.db().pool(), subject, &context, permission).await
            {
                Ok(Decision::Allowed(_)) => Ok(Caller::Machine(MachinePrincipal {
                    account: subject,
                    organization_id: machine.account.organization_id,
                    key_id: machine.key.id,
                    key_prefix: Some(machine.key.prefix.clone()),
                })),
                Ok(Decision::Denied { reason, source }) => {
                    tracing::debug!(
                        permission,
                        service_account = %machine.account.id,
                        ?reason,
                        role = source.map(|grant| grant.role_key),
                        "machine permission denied"
                    );
                    Err((
                        explain_denial(state, subject, &context, permission).await,
                        Some(principal),
                    ))
                }
                Err(error) => Err((ApiError::from(error), Some(principal))),
            };
        }
    }

    // Nothing authenticated at all. A `401` row names the permission that was asked for and
    // nothing else — there is no caller to attribute it to, and inventing one is the log lying.
    Err((
        ApiError::unauthorized("unauthenticated", "sign in to continue"),
        Some(crate::request_log_middleware::ResolvedPrincipal {
            permission: Some(permission.to_owned()),
            ..Default::default()
        }),
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
