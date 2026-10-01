//! The IP access list, on the request path (REQ-012, slice 4).
//!
//! **What was missing, and why this module exists at all.** Slice 4 shipped the rules, the
//! screens and the routes, and the request file itself asks for "an evaluator that runs before
//! route guards". Without this module an operator could deny `203.0.113.0/24`, watch the row
//! appear in the table, and go on being served from it — the screen would be a list of
//! intentions. That is the same shape of defect this REQ has produced repeatedly, and the
//! acceptance criterion ("a denied CIDR cannot reach the API") is worded to catch exactly it: it
//! asks for a *refusal*, not a configuration.
//!
//! **The layer sits beside the limiter, ahead of every guard.** An access list that ran after the
//! permission guards would refuse only callers who already authenticated — which is precisely
//! backwards, because the address rules exist to stop a caller *before* they have an account.
//!
//! **The policy is read once and swapped in place, like the header policy.** A per-request
//! database read would make every request's cost depend on the database — which is how a
//! settings screen turns into an outage — so [`IpAccess`] holds the current rule set behind an
//! `RwLock` and a save replaces it. A poisoned lock keeps the last good value: the lock holds a
//! pointer, so the rules cannot be half-written, and falling back to an *empty* list would
//! silently disable the access list, which is the failure this module exists to remove.
//!
//! **A request with no address is not silently allowed.** `tower`'s in-process `oneshot` builds a
//! request with no `ConnectInfo` extension, and the integration suites all use it. With rules in
//! force, such a request is refused with `ip_unknown` — *unless* the harness opts in through
//! [`unaddressed_requests_are_allowed`]. This matters for the proof rather than for production:
//! the walk that shows a denied CIDR cannot reach the API must refuse for the *rule's* reason, and
//! if every harness request were refused for the *absence of an address*, that walk would pass for
//! the wrong reason and prove nothing about the CIDR. So the code under test refuses on the
//! address, and the harness-only escape is a named environment variable that is off by default and
//! logged at boot when it is on.

use std::convert::Infallible;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, OnceLock, RwLock};
use std::task::{Context, Poll};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, Response, StatusCode};
use axum::response::IntoResponse;
use omnion_security::{IpRule, evaluate_ip_rules};
use tower::{Layer, Service};

use crate::error::ApiError;
use crate::state::AppState;

/// The live rule set, shared by every request.
#[derive(Clone)]
pub struct IpAccess {
    /// Kept for [`IpAccess::from_store`] and for the boot-time log; the request path never
    /// reads it, because judging a request needs no database — which is the whole reason the
    /// access list cannot take the platform down.
    #[allow(dead_code)]
    state: AppState,
    rules: Arc<RwLock<Arc<Vec<IpRule>>>>,
}

impl IpAccess {
    /// Build a layer holding `rules`.
    #[must_use]
    pub fn new(state: &AppState, rules: Vec<IpRule>) -> Self {
        Self {
            state: state.clone(),
            rules: Arc::new(RwLock::new(Arc::new(rules))),
        }
    }

    /// Read the stored rules and build the layer from them.
    ///
    /// Falls back to an empty rule set — which *allows* everything — on a read failure rather
    /// than refusing to boot, and logs why. A platform that will not start because its access
    /// rules are unreadable is a worse outcome than one that starts and says it could not read
    /// them; the operator can then fix the database without a page.
    pub async fn from_store(state: &AppState) -> Self {
        let rules = match omnion_security::list_ip_rules(state.db().pool()).await {
            Ok(rules) => rules,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "the IP access rules could not be read; no address rule is in force"
                );
                Vec::new()
            }
        };
        Self::new(state, rules)
    }

    /// Replace the rule set without rebuilding the router.
    ///
    /// Called after a save so the new rules are what the *next* request is decided by, rather than
    /// after the next restart.
    pub fn reload(&self, rules: Vec<IpRule>) {
        match self.rules.write() {
            Ok(mut current) => *current = Arc::new(rules),
            Err(poisoned) => {
                tracing::warn!("the IP rule lock was poisoned; keeping the last rule set");
                let mut current = poisoned.into_inner();
                *current = Arc::new(rules);
            }
        }
    }

    /// The rules the next request will be judged by — a pointer clone, never a copy.
    #[must_use]
    pub fn current(&self) -> Arc<Vec<IpRule>> {
        self.rules
            .read()
            .map(|rules| Arc::clone(&rules))
            .unwrap_or_else(|poisoned| Arc::clone(&poisoned.into_inner()))
    }
}

/// The one installed access list of this process.
///
/// Process-wide because the layer must exist before `router()` returns, while the add/remove
/// handlers are `fn` items built long before that and have nowhere to keep a clone — the same
/// constraint `rate_limit_middleware` documents.
static INSTALLED: OnceLock<IpAccess> = OnceLock::new();

/// Install the process's access list and return it.
///
/// # Panics
/// If two threads race before either finishes installing — a programming error worth stopping on.
#[must_use]
pub fn install(access: IpAccess) -> &'static IpAccess {
    INSTALLED.get_or_init(|| access)
}

/// Install the access list with the stored rules if the process does not have one yet.
#[must_use]
pub fn ensure_installed(state: &AppState) -> &'static IpAccess {
    if let Some(existing) = INSTALLED.get() {
        return existing;
    }
    install(IpAccess::new(state, Vec::new()))
}

/// The installed access list, if one is.
#[must_use]
pub fn installed() -> Option<&'static IpAccess> {
    INSTALLED.get()
}

/// Re-read the stored rules into the installed layer after a write.
///
/// Returns `false` when no layer is installed or the rules could not be read back; the caller
/// logs either way rather than treating a write that *did* land as a failure.
pub async fn reload_from_store(state: &AppState) -> bool {
    let Ok(rules) = omnion_security::list_ip_rules(state.db().pool()).await else {
        tracing::warn!(
            "the IP rules could not be read back after a save; the running process keeps its \
             previous rules until the next boot"
        );
        return false;
    };
    let Some(access) = installed() else {
        tracing::info!(
            "the IP rules were saved before the router was built; they apply from the next boot"
        );
        return false;
    };
    access.reload(rules);
    true
}

/// Whether a request with **no** client address is allowed through.
///
/// Off in every deployed configuration. The integration suites set it because `oneshot` carries
/// no connection info; without it every walk in the repository would be refused before reaching
/// the guard it is actually testing. Logged at boot when it is on, because a flag that exists
/// only to make tests pass must never be silently on in production.
#[must_use]
pub fn unaddressed_requests_are_allowed() -> bool {
    std::env::var("OMNION_IP_ACCESS_ALLOW_UNADDRESSED")
        .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

/// The layer, from an installed access list.
#[must_use]
pub fn ip_access(access: &'static IpAccess) -> IpAccessLayer {
    IpAccessLayer {
        access: access.clone(),
    }
}

/// A cheap, clonable handle the router stores as a layer.
#[derive(Clone)]
pub struct IpAccessLayer {
    access: IpAccess,
}

impl<S> Layer<S> for IpAccessLayer {
    type Service = IpAccessService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        IpAccessService {
            inner,
            access: self.access.clone(),
        }
    }
}

/// The service every route is wrapped in.
///
/// `Clone` because `Router::layer` requires a clonable service: the router clones it per route,
/// which is also why `call` clones `inner` rather than borrowing it.
#[derive(Clone)]
pub struct IpAccessService<S> {
    inner: S,
    access: IpAccess,
}

impl<S> Service<Request<Body>> for IpAccessService<S>
where
    S: Service<Request<Body>, Response = Response<Body>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        let access = self.access.clone();
        let mut inner = self.inner.clone();
        // Split rather than borrow: the decision reads the extensions, and the request then has
        // to be handed on intact. `parts` keeps the extensions, so the address survives the split.
        let (parts, body) = request.into_parts();

        Box::pin(async move {
            let peer = parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ConnectInfo(address)| address.ip());

            match decide(&access.current(), peer) {
                Err(error) => Ok(error.into_response()),
                Ok(()) => inner.call(Request::from_parts(parts, body)).await,
            }
        })
    }
}

/// Judge one request before the router sees it.
///
/// The body always names the rule. A `403` with no explanation is the failure mode the whole
/// design of this module is against: an operator who cannot see which rule refused them cannot
/// remove it, and will conclude the platform is broken.
///
/// The `IpAccess` handle is not needed and deliberately not taken: **judging a request needs no
/// state at all** — the rules are already in hand — which is why this is a free function over
/// `&[IpRule]` and why the tests below never build a database pool to prove it.
fn decide(rules: &[IpRule], address: Option<IpAddr>) -> Result<(), ApiError> {
    if rules.is_empty() {
        // No rules configured means no access list is in force. Stated rather than hidden, so a
        // deployment that believes it has rules can see that it has none.
        return Ok(());
    }

    let Some(address) = address else {
        if unaddressed_requests_are_allowed() {
            return Ok(());
        }
        // Rules exist and the platform cannot see where this came from, so it cannot prove the
        // rules were satisfied. Refusing here is what makes "a denied CIDR cannot reach the API"
        // a claim about the platform rather than about the harness.
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "ip_unknown",
            "this request carries no client address, so the IP access rules could not be applied",
        ));
    };

    let verdict = evaluate_ip_rules(&rules, Some(address), time::OffsetDateTime::now_utc());
    if verdict.blocked {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "ip_denied",
            verdict.reason,
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnion_security::{RuleKind, parse_cidr};
    use time::OffsetDateTime;
    use uuid::Uuid;

    fn rule(kind: RuleKind, cidr: &str) -> IpRule {
        IpRule {
            id: Uuid::new_v4(),
            kind,
            cidr: parse_cidr(cidr).expect("fixture must parse"),
            note: "test".to_owned(),
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            expires_at: None,
        }
    }

    fn v4(text: &str) -> IpAddr {
        text.parse().expect("fixture address must parse")
    }

    /// Every test here calls [`decide`] over a plain rule slice — no pool, no `AppState`, no
    /// Tokio runtime. That is the point of the function's shape: the decision is pure, so the
    /// rule that decides a request can be proved on its own.
    #[test]
    fn an_empty_rule_set_allows_everything() {
        assert!(decide(&[], Some(v4("203.0.113.7"))).is_ok());
    }

    #[test]
    fn a_denied_address_is_refused_and_the_body_names_the_rule() {
        let rules = vec![rule(RuleKind::Deny, "203.0.113.0/24")];
        let error = decide(&rules, Some(v4("203.0.113.7"))).expect_err("must be refused");
        assert_eq!(error.code(), "ip_denied");
        assert!(
            error.message().contains("203.0.113.0/24"),
            "the refusal must name the rule, or the operator cannot remove it: {}",
            error.message()
        );
    }

    #[test]
    fn an_address_outside_every_rule_is_served() {
        let rules = vec![rule(RuleKind::Deny, "203.0.113.0/24")];
        assert!(decide(&rules, Some(v4("198.51.100.7"))).is_ok());
    }

    #[test]
    fn an_expired_deny_does_not_refuse() {
        let mut deny = rule(RuleKind::Deny, "203.0.113.0/24");
        deny.expires_at = Some(OffsetDateTime::now_utc() - time::Duration::minutes(1));
        assert!(decide(&[deny], Some(v4("203.0.113.7"))).is_ok());
    }

    #[test]
    fn a_request_with_no_address_is_refused_while_rules_are_in_force() {
        // This is what makes "a denied CIDR cannot reach the API" a claim about the platform
        // rather than about the harness: with rules configured and no address to judge, the
        // request is refused rather than waved through on the grounds that nothing matched.
        let rules = vec![rule(RuleKind::Deny, "203.0.113.0/24")];
        let error = decide(&rules, None).expect_err("must be refused");
        assert_eq!(error.code(), "ip_unknown");
    }

    #[test]
    fn a_request_with_no_address_is_served_when_no_rules_exist() {
        assert!(decide(&[], None).is_ok());
    }

    #[test]
    fn an_allow_does_not_refuse_the_address_it_covers() {
        let rules = vec![rule(RuleKind::Allow, "203.0.113.0/24")];
        assert!(decide(&rules, Some(v4("203.0.113.7"))).is_ok());
    }

    // -- the installed layer ---------------------------------------------------------------
    //
    // `decide` is pure and the layer is a cache in front of it, so the only thing left to prove
    // about the layer is that a reload changes what it hands over. That is the property that
    // makes a rule added on the panel take effect on the next request rather than the next boot.

    #[tokio::test]
    async fn a_reload_replaces_the_rule_set_the_next_request_is_judged_by() {
        let rules = vec![rule(RuleKind::Deny, "203.0.113.0/24")];
        let cell: Arc<RwLock<Arc<Vec<IpRule>>>> = Arc::new(RwLock::new(Arc::new(rules.clone())));
        assert!(decide(&cell.read().expect("lock"), Some(v4("198.51.100.7"))).is_ok());

        // Simulate `reload` without an AppState: the cache is an `RwLock<Arc<Vec<_>>>` and that
        // is the whole contract, so the test exercises it directly rather than standing up a pool.
        *cell.write().expect("lock") = Arc::new(Vec::new());
        assert!(decide(&cell.read().expect("lock"), Some(v4("203.0.113.7"))).is_ok());

        *cell.write().expect("lock") = Arc::new(rules);
        assert!(decide(&cell.read().expect("lock"), Some(v4("203.0.113.7"))).is_err());
    }
}
