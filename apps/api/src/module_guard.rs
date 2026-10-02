//! Per-organization module enforcement (docs/requests/REQ-005, slice 4).
//!
//! The Modules tab stores a decision — `organization_modules` says whether *this* tenant uses the
//! AI Hub, the media library, analytics, automations or webhooks. Storing it is not the same as
//! applying it, and the REQ is explicit about what applying it means: "switching a module off for
//! an organization hides its navigation entry and makes its API answer 403 naming the module".
//!
//! **One layer, one table, mounted once.** The obvious shape is a guard per module, applied to
//! that module's routes: it puts a mount per module in the router *and* a route list per module
//! in this file, and a screen added to `media` and not to the `media` guard is a screen the switch
//! does not govern — with nothing failing when that happens, because the test that would catch it
//! has to be written by whoever added the screen. So the guard is mounted once on the `/api/v1`
//! router and asks [`tenancy_limits::route_module`] which module owns the matched path, and that
//! answer comes from the same table the panel's navigation is built from. A new screen is governed
//! the moment its prefix is in the table, and the only way to leave it unguarded is to leave it
//! out of the table — one visible line rather than a route somebody forgot to touch.
//!
//! Two rules decide the rest:
//!
//! * **The refusal names the module.** `403 organization.module.disabled` carries `module` and
//!   `module_name`, because "not authorized" sends an operator to the role screen while the
//!   actual cause is a switch an administrator flipped on the Modules tab.
//! * **A platform-level account is never refused.** It belongs to no tenant, so it has no module
//!   decisions; refusing its access would make the platform unable to administer the module it
//!   just switched off. The same reasoning as the tenant freeze — the control that undoes a
//!   restriction cannot be subject to it.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::extract::MatchedPath;
use axum::http::HeaderMap;
use axum::http::request::Request;
use axum::response::{IntoResponse, Response};
use omnion_identity::tenancy_limits;
use tower::{Layer, Service};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

/// Layer refusing a request whose organization switched the owning module off.
#[derive(Clone)]
pub struct RequireModules {
    state: AppState,
}

impl RequireModules {
    /// Build the layer.
    #[must_use]
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

impl<S> Layer<S> for RequireModules {
    type Service = RequireModulesService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequireModulesService {
            inner,
            state: self.state.clone(),
        }
    }
}

/// Service produced by [`RequireModules`].
#[derive(Clone)]
pub struct RequireModulesService<S> {
    inner: S,
    state: AppState,
}

/// What the module check decided about one request.
///
/// Put into the request extensions so a walk can tell "the module guard refused" from "the route
/// is not there" and from "the permission guard refused" — three answers that look identical from
/// the outside and are proven by three different walks.
///
/// It records only what a request *passed*: a disabled module never reaches an insert, so a
/// `disabled` flag here could only ever be `false`, and a field that is always `false` is worse
/// than no field — it is an answer to a question nobody asks, waiting for somebody to trust it.
#[derive(Debug, Clone, Copy)]
pub struct ModuleAccess {
    organization_id: Option<Uuid>,
}

impl ModuleAccess {
    /// The organization the check ran against, or `None` for a platform account, a path no
    /// module owns, or a request with no session.
    #[must_use]
    pub fn organization_id(&self) -> Option<Uuid> {
        self.organization_id
    }
}

impl<S> Service<Request<Body>> for RequireModulesService<S>
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
        let mut inner = self.inner.clone();

        Box::pin(async move {
            // The *matched* path is the route as written (`/media/{id}/raw`), which is what the
            // table is keyed on. The raw URI is the fallback for a request that matched nothing
            // — a 404 — where the answer is `Ok` either way, but the two still have to agree on
            // which module it was.
            let path = request.extensions().get::<MatchedPath>().map_or_else(
                || request.uri().path().to_owned(),
                |matched| matched.as_str().to_owned(),
            );
            let headers = request.headers().clone();

            match check(&state, &path, &headers).await {
                Ok((organization_id, session)) => {
                    let extensions = request.extensions_mut();
                    // Only *add* the session, never replace one: the permission guard behind puts
                    // its own resolution in the same map, and overwriting it would swap a machine
                    // principal's session for a stale one. `insert` here would be a race with a
                    // layer this one does not control.
                    if let Some(session) = session {
                        if !extensions.get::<crate::auth::CurrentSession>().is_some() {
                            extensions.insert(session);
                        }
                    }
                    extensions.insert(ModuleAccess { organization_id });
                    inner.call(request).await
                }
                Err(error) => Ok(error.into_response()),
            }
        })
    }
}

/// Decide whether a request may reach the module that owns its path.
///
/// `Ok` for every path no module owns, for a caller with no tenant (a platform account), for a
/// request with no session at all (the permission guard behind answers for that) and for a
/// module that is on. `Err` is the refusal naming the module.
///
/// The resolved session is handed back so the permission guard behind can reuse it: this layer
/// is *in front of* every `guards::require`, which means without that hand-off every request to
/// a module route would resolve its session twice — and the analytics screens are the ones that
/// ask the most questions per screen.
async fn check(
    state: &AppState,
    path: &str,
    headers: &HeaderMap,
) -> Result<(Option<Uuid>, Option<crate::auth::CurrentSession>), ApiError> {
    let Some(module) = tenancy_limits::route_module(path) else {
        return Ok((None, None));
    };

    // Resolution failures are swallowed on purpose: a request with no session here is a request
    // the permission guard behind is about to refuse with `401`, and refusing it here with a
    // *module* reason would blame the wrong thing. A machine-key route — where the guard
    // resolved a principal and not a session — likewise falls through untouched.
    let session = match crate::auth::CurrentSession::resolve(state, headers).await {
        Ok(session) => session,
        Err(_) => return Ok((None, None)),
    };
    let Some(organization_id) = session.user.organization_id else {
        return Ok((None, Some(session)));
    };

    if tenancy_limits::is_module_enabled(state.db().pool(), organization_id, module).await? {
        return Ok((Some(organization_id), Some(session)));
    }

    Err(refused(module))
}

/// The refusal a disabled module answers with, naming the module and where it comes back.
#[must_use]
pub fn refused(module: &str) -> ApiError {
    let name = tenancy_limits::installed_modules()
        .into_iter()
        .find(|installed| installed.key == module)
        .map_or_else(|| module.to_owned(), |installed| installed.name.to_owned());

    ApiError::new(
        axum::http::StatusCode::FORBIDDEN,
        "organization.module.disabled",
        format!(
            "the {name} module is switched off for this organization; an administrator can turn \
             it back on in the organization's Modules tab"
        ),
    )
    .with_details(serde_json::json!({
        "module": module,
        "module_name": name,
        "turn_on_in": "the organization's Modules tab",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disabled_module_is_named_in_the_refusal() {
        let refusal = refused("media");
        assert_eq!(refusal.code(), "organization.module.disabled");
        assert_eq!(refusal.status(), axum::http::StatusCode::FORBIDDEN);

        let details = refusal.details().expect("the refusal carries details");
        assert_eq!(details["module"], "media");
        assert_eq!(
            details["module_name"], "Media library",
            "the refusal carries the product name, not just the key: `media` alone reads as a \
             path segment to somebody who has never seen the Modules tab"
        );
    }

    #[test]
    fn a_module_the_installation_does_not_ship_still_answers() {
        // The key comes from the table rather than a hard-coded list, so a typo must not produce
        // a panic or a refusal with an empty name.
        let refusal = refused("quantum-hub");
        assert_eq!(
            refusal.details().expect("details")["module_name"],
            "quantum-hub"
        );
    }

    #[test]
    fn every_module_route_belongs_to_an_installed_module() {
        // The two lists sit side by side in the crate; this is the assertion that stops a module
        // from being routable but not listed — a switch with nothing behind it.
        for route in tenancy_limits::module_routes() {
            assert!(
                tenancy_limits::installed_modules()
                    .iter()
                    .any(|installed| installed.key == route.module),
                "the route table names {}, which the installation does not ship",
                route.module
            );
        }
    }

    #[test]
    fn the_path_a_route_is_mounted_on_decides_its_module() {
        // The mount prefixes are part of the same table, so a walk that navigates the panel's
        // URL and a request that hits the API agree by construction. The assertion is written
        // from the panel's side on purpose: it is the side a person reaches.
        for route in tenancy_limits::module_routes() {
            assert_eq!(
                tenancy_limits::route_module(route.path_prefix),
                Some(route.module),
                "the panel's own entry for {} does not resolve to the module that guards it",
                route.path_prefix
            );
        }
    }
}
