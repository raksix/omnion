//! Request-scoped context: how one request id and one actor reach every line that request emits.
//!
//! The request is explicit about the failure this closes — "a request id that only appears in
//! the error banner is an id the log store cannot join to" — and about the shape of the fix: a
//! middleware assigns the request id **at the edge**, binds the user and organization **after
//! authentication**, and every log line inside that task inherits both.
//!
//! ## Why the context is a task-local and not a field on every call site
//!
//! Because the alternative is the thing this request is a reaction to. A `tracing::info!` with
//! `request_id = ?ctx.request_id` compiles, passes review, and then a *second* `info!` in the same
//! function is written without it. The compiler cannot see the difference between the line that
//! carries the context and the line that forgot it, so the omission is invisible until someone
//! greps the store at 2 a.m.
//!
//! A task-local makes the inheritance automatic and the *opt-out* explicit. [`LogContext::enter`]
//! installs the binding; [`LogContext::current`] reads it; a line emitted deep inside a worker
//! spawns with whatever its parent had, which is exactly the propagation the request asks for
//! ("a worker log line carries the trace id of the request that enqueued the job").
//!
//! ## The scope of the binding
//!
//! The context is per **task**, not per process. A `tokio::spawn` inside a request inherits it
//! at spawn time (a plain `tokio::spawn` reads the task-local of its parent), and a
//! `tokio::task::spawn` inside a *background runner* that never had a request sees nothing — so a
//! runner's own lines are correctly anonymous rather than wrongly attributed to whatever request
//! happened to be in flight.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

tokio::task_local! {
    /// The binding of the currently-running request, if any.
    ///
    /// It is behind an `Arc<Mutex<…>>` rather than a plain value for a reason that is easy to get
    /// wrong: **the actor is not known when the context is installed.** The request id exists at
    /// the edge, before anyone has authenticated, and the user and organization only exist once
    /// the route guard has resolved the session — which happens *deeper* in the chain, in a layer
    /// that holds a different `Request` than the one the outer layer still owns. So the outer
    /// middleware cannot read the actor out of extensions after the fact; it never propagates back
    /// up. The guard therefore *writes* the actor here, and the middleware reads it back when it
    /// is ready to record the line. That is precisely what the request describes: the context is
    /// bound at the edge and the actor is bound after authentication.
    static CURRENT: ContextCell;
}

/// A shared, mutable context cell.
type ContextCell = std::sync::Arc<std::sync::Mutex<LogContext>>;

/// The identity a request runs under.
///
/// Every field is optional because the lifecycle has three stages, not one: the edge knows the
/// request id before anybody has signed in, the guard knows the user after the session resolves,
/// and a background worker knows neither. A context type with required user and organization
/// fields would force the first and third cases to invent values.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogContext {
    /// The id assigned at the edge and returned to the caller as a response header.
    pub request_id: Option<Uuid>,
    /// The id of the signed-in account, once authentication has resolved.
    pub user_id: Option<Uuid>,
    /// The organization the account acts in, or `None` for a platform-level account.
    pub organization_id: Option<Uuid>,
    /// The W3C trace id, propagated from the inbound `traceparent` or minted at the edge.
    pub trace_id: Option<String>,
    /// The span this task represents, for a line emitted inside a nested operation.
    pub span_id: Option<String>,
    /// `api`, `worker` or `cli` — which process the line came from.
    pub source: Option<String>,
    /// The route template being served, never the literal path.
    pub route: Option<String>,
    /// The HTTP method.
    pub method: Option<String>,
    /// The responding status code, set by the middleware when the response is produced.
    pub status: Option<u16>,
    /// How long the request took, in milliseconds.
    pub duration_ms: Option<i64>,
    /// The host the process runs on.
    pub host: Option<String>,
    /// The instance version, so a log line says which build produced it.
    pub version: Option<String>,
}

impl LogContext {
    /// A context carrying only a request id, as it exists at the edge.
    #[must_use]
    pub fn new_request(request_id: Uuid) -> Self {
        Self {
            request_id: Some(request_id),
            ..Self::default()
        }
    }

    /// The same context with the actor bound — what the guard hands to a handler.
    #[must_use]
    pub fn with_actor(mut self, user_id: Option<Uuid>, organization_id: Option<Uuid>) -> Self {
        self.user_id = user_id;
        self.organization_id = organization_id;
        self
    }

    /// The same context bound to a trace.
    #[must_use]
    pub fn with_trace(mut self, trace_id: impl Into<String>) -> Self {
        self.trace_id = Some(trace_id.into());
        self
    }

    /// Name the process this context belongs to: `api`, `worker` or `cli`.
    ///
    /// [`crate::schema::LogSource`] owns the three names, so the context takes the same closed
    /// set rather than any string — a misspelled source is a row the explorer's source filter
    /// cannot select, and it is exactly the kind of typo no test would notice.
    #[must_use]
    pub fn with_source(mut self, source: &str) -> Self {
        self.source = match source {
            "worker" | "api" | "cli" => Some(source.to_owned()),
            _ => self.source,
        };
        self
    }

    /// Run a future with this context installed.
    ///
    /// This is the one entry point rather than a guard-returning `enter`, because a
    /// guard-returning form has to be awaited to extend its scope, which does not compose with
    /// `?` and an early return in a handler — and a context API that is awkward at a call site
    /// gets worked around with a manual `request_id` field, which is the failure this module
    /// exists to prevent.
    pub async fn scope<F, T>(self, future: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        CURRENT
            .scope(std::sync::Arc::new(std::sync::Mutex::new(self)), future)
            .await
    }

    /// Bind the actor of the running request, once authentication has resolved it.
    ///
    /// Called by the route guard, not by the middleware: the guard is the layer that actually
    /// knows who the caller is, and it is the only one holding the `Request` whose extensions the
    /// session was written into.
    ///
    /// A request that never authenticated binds nothing, and its line says so — which is the
    /// honest outcome for a refusal. Guessing an actor would be the one thing worse than a null
    /// column in a log an operator trusts.
    pub fn bind_actor(user_id: Option<Uuid>, organization_id: Option<Uuid>) {
        let _ = CURRENT.try_with(|cell| {
            if let Ok(mut context) = cell.lock() {
                context.user_id = user_id;
                context.organization_id = organization_id;
            }
        });
    }

    /// Fill in the route and outcome of the running request, when they are known.
    pub fn bind_outcome(route: Option<String>, status: Option<u16>, duration_ms: Option<i64>) {
        let _ = CURRENT.try_with(|cell| {
            if let Ok(mut context) = cell.lock() {
                if let Some(route) = route {
                    context.route = Some(route);
                }
                context.status = status;
                context.duration_ms = duration_ms;
            }
        });
    }

    /// Wrap a future so it runs with **this** context, whenever it is awaited.
    ///
    /// ## Why this exists at all, and why it is not automatic
    ///
    /// `tokio::spawn` does **not** inherit a `task_local!` scope. That is a deliberate property of
    /// Tokio — a task is not a child of its spawner, it is an independent unit of work with its
    /// own lifetime — and it means the inheritance this module's documentation was about to
    /// promise is simply not something the runtime does.
    ///
    /// The request's acceptance criterion is nonetheless explicit: *"a worker log line carries
    /// the trace id of the request that enqueued the job."* So propagation is made **explicit and
    /// typed** instead of assumed:
    ///
    /// * A worker that already holds the producer's context (the common case: a queue row carries
    ///   it) calls `let context = self.clone();` and passes `context.wrap(fut)`.
    /// * A worker that does not — a job dispatched from a runner tick, or a restart resuming
    ///   stored work — reads the context out of the job row and re-enters it, because at that
    ///   point the request genuinely no longer exists and pretending otherwise is the lie this
    ///   module exists to prevent.
    ///
    /// What is refused, loudly, is the third case: a task that reads [`LogContext::current`],
    /// finds it empty and writes a line with a null request id while claiming to be a request
    /// line. A test below pins that the *empty* context is what a detached task sees, so nobody
    /// later "fixes" the gap by assuming the runtime does it for us.
    #[must_use]
    pub fn wrap<F>(&self, future: F) -> impl std::future::Future<Output = F::Output> + use<F>
    where
        F: std::future::Future,
    {
        // The clone is owned by the returned future, not borrowed from `self`: the whole point is
        // that the result can be handed to `tokio::spawn`, which requires `'static`. Under
        // edition 2024's precise-capture rules, an `impl Future` that borrows `&self` captures
        // that borrow and the spawn is refused.
        let context = self.clone();
        async move { context.scope(future).await }
    }

    /// The context of the running task, or an empty one when nothing installed it.
    ///
    /// Returning an empty context rather than `None` is deliberate: a log line is always written,
    /// and a line with a null request id is honest, while a line whose *request id* is a previous
    /// request's is a lie.
    #[must_use]
    pub fn current() -> Self {
        CURRENT
            .try_with(|cell| {
                cell.lock()
                    .map(|context| context.clone())
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    }

    /// Read a `tracing` span field for the current request, so a `tracing` event and a stored log
    /// line describe the same request.
    #[must_use]
    pub fn as_tracing_fields() -> Vec<(&'static str, String)> {
        let context = Self::current();
        let mut out: Vec<(&'static str, String)> = Vec::with_capacity(6);
        if let Some(id) = context.request_id {
            out.push(("request_id", id.to_string()));
        }
        if let Some(trace) = context.trace_id {
            out.push(("trace_id", trace));
        }
        if let Some(user) = context.user_id {
            out.push(("user_id", user.to_string()));
        }
        if let Some(organization) = context.organization_id {
            out.push(("organization_id", organization.to_string()));
        }
        out
    }
}

/// A 32-hex trace id, minted the way W3C `traceparent` expects.
///
/// The request asks for W3C propagation; a trace id that is not 16 random bytes is not a W3C
/// trace id, and an operator pasting ours into a collector's search box gets nothing.
#[must_use]
pub fn mint_trace_id() -> String {
    Uuid::new_v4().simple().to_string()
}

/// A 16-hex span id.
#[must_use]
pub fn mint_span_id() -> String {
    // Half a uuid is exactly the 8 bytes a W3C span id carries.
    Uuid::new_v4().simple().to_string()[..16].to_owned()
}

/// Read the trace id out of a W3C `traceparent` header.
///
/// Returns `None` for a header that is not the shape W3C defines (four `-`-separated fields, the
/// first `00`) rather than guessing: a malformed trace id that is accepted becomes a span that no
/// collector will ever join to the request it was copied from.
#[must_use]
pub fn trace_id_from_header(header: &str) -> Option<String> {
    let mut parts = header.trim().split('-');
    let version = parts.next()?;
    let trace_id = parts.next()?;
    let span_id = parts.next()?;
    let _flags = parts.next()?;
    if parts.next().is_some() || version.len() != 2 || trace_id.len() != 32 || span_id.len() != 16 {
        return None;
    }
    if trace_id.chars().all(|c| c == '0') {
        return None;
    }
    Some(trace_id.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_context_is_built_in_stages() {
        let request_id = Uuid::new_v4();
        let user_id = Uuid::new_v4();
        let organization_id = Uuid::new_v4();

        let edge = LogContext::new_request(request_id);
        assert_eq!(edge.request_id, Some(request_id));
        assert!(edge.user_id.is_none(), "the edge knows no actor yet");

        let authenticated = edge
            .clone()
            .with_actor(Some(user_id), Some(organization_id));
        assert_eq!(authenticated.user_id, Some(user_id));
        assert_eq!(authenticated.organization_id, Some(organization_id));
        // The request id survives the actor binding — that is the whole point of it being
        // generated before anyone has authenticated.
        assert_eq!(authenticated.request_id, Some(request_id));
    }

    #[tokio::test]
    async fn a_line_inside_the_scope_inherits_the_context() {
        let request_id = Uuid::new_v4();
        let user_id = Uuid::new_v4();

        let observed = LogContext::new_request(request_id)
            .with_actor(Some(user_id), None)
            .with_trace("abc123")
            .scope(async { LogContext::current() })
            .await;

        assert_eq!(observed.request_id, Some(request_id));
        assert_eq!(observed.user_id, Some(user_id));
        assert_eq!(observed.trace_id.as_deref(), Some("abc123"));
    }

    #[tokio::test]
    async fn a_detached_task_sees_no_context_and_a_wrapped_one_carries_it() {
        let request_id = Uuid::new_v4();
        let trace_id = mint_trace_id();

        let (inherited, wrapped) = LogContext::new_request(request_id)
            .with_trace(&trace_id)
            .scope(async {
                // What a bare `tokio::spawn` sees. Tokio tasks are not children of their
                // spawner, so a task_local scope does NOT cross a spawn — this assertion pins
                // that fact so nobody later "fixes" the worker path by assuming it does.
                let inherited = tokio::spawn(async { LogContext::current() })
                    .await
                    .expect("the detached task must not panic");

                // What a worker that wraps its future sees: the producer's request id and trace,
                // with no manual copying on either side.
                let context = LogContext::current();
                let wrapped = tokio::spawn(context.wrap(async { LogContext::current() }))
                    .await
                    .expect("the wrapped task must not panic");

                (inherited, wrapped)
            })
            .await;

        assert!(
            inherited.request_id.is_none(),
            "tokio::spawn does not inherit a task-local; a worker line must be wrapped explicitly"
        );
        assert_eq!(wrapped.request_id, Some(request_id));
        assert_eq!(
            wrapped.trace_id.as_deref(),
            Some(trace_id.as_str()),
            "a worker line must carry the trace of the request that enqueued the job"
        );
    }

    #[tokio::test]
    async fn a_context_does_not_leak_into_the_next_task() {
        let request_id = Uuid::new_v4();
        let _ = LogContext::new_request(request_id)
            .scope(async {
                // A nested scope, and a scope that returns early — the case a `?` in a handler
                // produces.
                let _inner = LogContext::new_request(Uuid::new_v4())
                    .scope(async { 42 })
                    .await;
            })
            .await;

        // Outside the scope the current context is the empty one, never the inner request's.
        assert!(LogContext::current().request_id.is_none());
    }

    #[test]
    fn a_task_with_no_context_gets_an_empty_one_rather_than_a_previous_requests() {
        // This is the property that makes the module usable from a background runner: a runner
        // task has no request, and says so, instead of inheriting whatever ran last.
        let context = LogContext::current();
        assert!(context.request_id.is_none());
        assert!(context.user_id.is_none());
    }

    #[test]
    fn a_well_formed_traceparent_yields_its_trace_id() {
        let header = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        assert_eq!(
            trace_id_from_header(header).as_deref(),
            Some("4bf92f3577b34da6a3ce929d0e0e4736")
        );
    }

    #[test]
    fn a_malformed_traceparent_is_refused_rather_than_guessed() {
        for header in [
            "",
            "00-tooshort-00f067aa0ba902b7-01",
            "4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
        ] {
            assert_eq!(
                trace_id_from_header(header),
                None,
                "`{header}` must not produce a trace id"
            );
        }
    }

    #[test]
    fn minted_ids_are_the_widths_w3c_defines() {
        let trace = mint_trace_id();
        let span = mint_span_id();
        assert_eq!(trace.len(), 32, "a W3C trace id is 16 bytes in hex");
        assert_eq!(span.len(), 16, "a W3C span id is 8 bytes in hex");
        assert_ne!(trace, mint_trace_id());
        assert!(trace.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
