//! The request id a trail line and an audit line carry (REQ-117, acceptance 17).
//!
//! ## Why this exists
//!
//! Acceptance 17 asks for an audit entry with "actor, before/after **and request id**". Actor and
//! before/after shipped; this is the third fact, and without it an audit line answers *who did
//! this* but not *which exchange did it*, so two edits made by the same operator in the same
//! minute are indistinguishable in a compliance export. The REQ names the alternative and this is
//! why it was not taken: a header-shaped column that is never populated reads as recorded and is
//! not, which is worse than an absent one.
//!
//! ## The seam, and why it is task-local rather than a parameter
//!
//! The trail lines are written deep in the store — `assign_owner` appends inside its own
//! transaction, `record_response` appends after the update, `convert_lead` appends from the
//! conversion report. Threading a `request_id: Option<&str>` through all of them touches every
//! call site in the crate **and every test**, and a parameter nobody can forget at the call site
//! is a parameter somebody eventually forgets. Worse, the honest failure mode of forgetting it is
//! silent: the line is written and the id is `None`, and the column says the exchange was not
//! recorded while the row looks perfectly healthy.
//!
//! So the id travels in a task-local cell instead, set once by the HTTP layer at the top of the
//! exchange and read by whatever writes a line below it:
//!
//! * **Task-local, not thread-local.** A `thread_local!` would be wrong in the most confusing way
//!   available: `tokio::spawn` moves a future to a different worker thread, so a thread-local set
//!   before the spawn reads as `None` inside it. `tokio::task_local!` follows the future.
//! * **But a task-local does not cross `tokio::spawn` either**, and that was measured, not assumed
//!   — see [`a_spawned_task_does_not_inherit_the_id_and_says_so`]. A spawn site that writes a trail
//!   line re-opens the scope with [`scope_with`], which carries the id in as an argument
//!   of hoping the runtime carried it. The alternative — trusting that a task-local behaves like a
//!   thread-local that behaves like a captured variable — is the kind of assumption that passes
//!   every test that stays on one task.
//! * **Absent is a value, not an error.** `None` is the correct answer for a worker sweep and for
//!   a test that calls the store directly, so `current()` never fails and never invents one.
//!
//! ## What is *not* here
//!
//! Minting and validating the header belongs to the API's request-id middleware, which lives
//! outside this module and is shared with every other route. This file only **consumes** that
//! value. Minting an id here would mean a second, different id for the same exchange: the one on
//! the response header and the one in the audit row would not match, and the entire value of a
//! correlation id is that they do.

use std::cell::RefCell;

use tokio::task_local;

// The id of the exchange the current task is serving.
//
// Task-local so it travels with the future: `assign_owner` is awaited on this task, its trail
// line is appended on this task, and the `convert` handler's spawned autoresponder reservation is
// *not* — which is correct, because that work is not part of the operator's exchange.
//
// Two things about the form of this declaration, both learned by getting them wrong:
//
// * **No initialiser.** `task_local!` declares a *type* and nothing else — `static NAME: TYPE;` —
//   and the value for each scope is passed to `CURRENT.scope(..)`. An initialiser is a macro
//   error, not a style question.
// * **No doc comment.** A `///` here attaches to the macro *invocation*, which rustdoc documents
//   no such thing for, so it is reported as an unused doc comment on every build. The module's
//   own `//!` header above is where the explanation belongs.
task_local! {
    static CURRENT: RefCell<Option<String>>;
}

/// Run `future` with `request_id` as the current exchange's id.
///
/// Re-entrant: a scope inside a scope restores the outer id on the way out, so a handler that
/// calls into another scoped path does not leave the outer exchange's id behind for whatever runs
/// next.
pub async fn scope<F>(request_id: Option<String>, future: F) -> F::Output
where
    F: std::future::Future,
{
    CURRENT.scope(RefCell::new(request_id), future).await
}

/// Run `future` inside a scope carrying the exchange id it was **given**, for a `tokio::spawn`.
///
/// ## Why this exists
///
/// **A task-local does not cross `tokio::spawn`, and that was measured rather than assumed** — the
/// test `a_spawned_task_does_not_inherit_the_id_and_says_so` asserts `None` there, and it asserted
/// it the *first* time because the alternative was written down as a belief and believed. A spawned
/// task starts with an empty cell, so any trail line it writes would carry `request_id: null`.
///
/// That matters because the capture handler deliberately spawns the autoresponder *off* the
/// request (a visitor's `202` must not wait on an SMTP handshake) and that task writes its own
/// trail line. Left alone, the trail would show the lead's arrival correlated and its
/// acknowledgement uncorrelated — the one line an operator would most want to tie back to the
/// submission that caused it.
///
/// **So the id is captured at the spawn site and moved in.** Reading [`current`] *inside* the
/// spawned task cannot work — that is the whole finding — which is why this takes the value rather
/// than looking it up. A spawn site with no exchange to name passes `None`, which is the truth
/// about a worker sweep.
pub async fn scope_with<F>(request_id: Option<String>, future: F) -> F::Output
where
    F: std::future::Future,
{
    CURRENT.scope(RefCell::new(request_id), future).await
}

/// The current exchange's id, or `None` outside any scope.
///
/// The seam is deliberately not required to be set. A call from a worker has no request, a test
/// calls the store with none, and refusing to write the line because a correlation id was missing
/// would trade a complete audit trail for a complete correlation — and lose the first.
#[must_use]
pub fn current() -> Option<String> {
    CURRENT
        .try_with(|cell| cell.borrow().clone())
        .ok()
        .flatten()
}

/// [`current`], wrapped for storage in a JSON detail object.
///
/// Returns `serde_json::Value::Null` rather than omitting the key: a detail whose `request_id`
/// reads `null` says "this exchange was not correlated", and a detail with no `request_id` at all
/// cannot be told apart from one written before this field existed.
#[must_use]
pub fn detail_value() -> serde_json::Value {
    match current() {
        Some(id) => serde_json::Value::String(id),
        None => serde_json::Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn outside_a_scope_there_is_no_id_rather_than_an_invented_one() {
        // A worker sweep is the case this exists for. Inventing an id here would put a value in
        // the audit row that correlates with nothing, which is the failure this whole module is
        // about.
        assert_eq!(current(), None);
        assert_eq!(detail_value(), serde_json::Value::Null);
    }

    #[tokio::test]
    async fn a_scope_reads_its_own_id() {
        scope(Some("req-abc123".to_owned()), async {
            assert_eq!(current().as_deref(), Some("req-abc123"));
            assert_eq!(detail_value(), serde_json::Value::String("req-abc123".into()));
        })
        .await;
    }

    #[tokio::test]
    async fn the_scope_ends_with_the_task() {
        // A leak would be the worst shape of this bug: one request's id written onto another
        // request's audit line, with nothing in either row to notice.
        scope(Some("req-first".to_owned()), async {}).await;
        assert_eq!(current(), None);
    }

    #[tokio::test]
    async fn an_inner_scope_restores_the_outer_one() {
        scope(Some("req-outer".to_owned()), async {
            scope(Some("req-inner".to_owned()), async {
                assert_eq!(current().as_deref(), Some("req-inner"));
            })
            .await;
            // Re-entrancy is the half that a plain "set it and clear it" gets wrong: the inner
            // scope's clear would take the outer id with it.
            assert_eq!(current().as_deref(), Some("req-outer"));
        })
        .await;
    }

    #[tokio::test]
    async fn an_inner_scope_without_an_id_does_not_shadow_the_outer_one() {
        // The scope has to be told explicitly that there is no id, or a caller that merely omits
        // the value would blank the exchange the outer scope established.
        scope(Some("req-outer".to_owned()), async {
            scope(None, async {
                assert_eq!(current(), None);
            })
            .await;
            assert_eq!(current().as_deref(), Some("req-outer"));
        })
        .await;
    }

    #[tokio::test]
    async fn a_spawned_task_does_not_inherit_the_id_and_says_so() {
        // **Proved by this test failing**, which is why it is in the file. A task-local looks like
        // it should follow a future into `tokio::spawn`, and it does not: the spawned task gets a
        // fresh, empty cell and `current()` reads `None`.
        //
        // That is a property of tokio, not a bug here — and the consequence is concrete: the
        // capture handler spawns the autoresponder off the request, and that task writes its own
        // trail line. Left alone, that line would read `request_id: null` while the row that
        // caused it carries an id, and an operator comparing the two would reasonably conclude
        // the second line came from a different, uncorrelated exchange.
        //
        // So the boundary is documented instead of assumed, and the two spawn sites that write a
        // trail line re-open the scope themselves. See `request_id::scope_with`.
        scope(Some("req-parent".to_owned()), async {
            let seen = tokio::spawn(async { current() }).await.expect("the task runs");
            assert_eq!(seen, None, "tokio::task_local does not cross spawn");

            // And the repair, in the shape a call site actually has: read the id *before* the
            // spawn, then move it in. Reading it inside — the obvious one-liner — is the mistake
            // this file was written after making.
            let carried = current();
            let repaired = tokio::spawn(async move {
                scope_with(carried, async { current() }).await
            })
            .await
            .expect("the task runs");
            assert_eq!(repaired.as_deref(), Some("req-parent"));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_exchanges_on_two_threads_do_not_see_each_other() {
        // The cross-talk case the previous test cannot reach: a server serving two requests at once
        // would write one request's id onto the other's line if the cell were shared.
        let first = tokio::spawn(async { scope(Some("req-one".to_owned()), async { current() }).await });
        let second =
            tokio::spawn(async { scope(Some("req-two".to_owned()), async { current() }).await });
        assert_eq!(first.await.expect("first").as_deref(), Some("req-one"));
        assert_eq!(second.await.expect("second").as_deref(), Some("req-two"));
    }
}
