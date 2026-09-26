//! The host action handler: how a process gives the engine the ability to touch the world.
//!
//! The engine's synthetic actions are pure functions, but a platform's automations also need to
//! send an email and write a comment. Those are *host* actions (see [`crate::actions`]): the
//! engine knows their names, validates their parameters and refuses to run them alone, and the
//! process that owns the engine installs a handler for them.
//!
//! The split is deliberate (docs/09-N8N-TEARDOWN.md §13 lesson 14): the core process contains
//! no user-supplied code and no vendor client it does not need. The automation layer
//! (`omnion-automation`) implements this trait; a process that installs nothing still runs every
//! definition whose actions are synthetic, and fails a host action with an honest message
//! instead of pretending it worked.
//!
//! The handler is a trait object with a boxed future rather than an `async fn`, so the engine
//! can take `&dyn ActionHandler` through the runner's tick without going generic.

use std::future::Future;
use std::pin::Pin;

use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

/// Everything a host action needs to know about the step it is running.
///
/// The context carries the run and the tenant, never the workflow's definition: what an action
/// does is fully described by its (already resolved) parameters, so a retry of the same step
/// does exactly the same thing.
pub struct ActionContext<'a> {
    /// The platform's database pool.
    pub pool: &'a PgPool,
    /// Organization the run belongs to.
    pub organization_id: Uuid,
    /// Site of the workflow, when it is site-scoped.
    pub site_id: Option<Uuid>,
    /// The run the step belongs to.
    pub execution_id: Uuid,
    /// The step being run.
    pub step_id: Uuid,
    /// Position of the step in its run (1-based).
    pub step_no: i32,
    /// Attempt number of this claim, counting from 1.
    pub attempt: i32,
}

/// The future a host action returns: a step output, or the message of a failure.
pub type ActionFuture<'a> =
    Pin<Box<dyn Future<Output = std::result::Result<Value, String>> + Send + 'a>>;

/// The process's implementation of the engine's host actions.
pub trait ActionHandler: Send + Sync {
    /// Run one host action.
    ///
    /// A returned `Err` is the failure message the step records — the engine decides whether
    /// that means another attempt or a failed run, exactly as it does for a synthetic action.
    fn execute<'a>(
        &'a self,
        action: &'a str,
        params: &'a Value,
        context: &'a ActionContext<'_>,
    ) -> ActionFuture<'a>;
}

/// The handler of a process that installs none.
///
/// Every host action fails with the reason, so a misconfigured deployment is visible in the
/// step's error instead of leaving a run that silently did nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoActionHandler;

impl ActionHandler for NoActionHandler {
    fn execute<'a>(
        &'a self,
        action: &'a str,
        _params: &'a Value,
        _context: &'a ActionContext<'_>,
    ) -> ActionFuture<'a> {
        Box::pin(async move {
            Err(format!(
                "`{action}` needs an action handler; this process installed none"
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_process_without_a_handler_fails_host_actions_honestly() {
        // The default handler never succeeds, and it names the action it could not run.
        let pool = PgPool::connect_lazy("postgres://omnion@127.0.0.1/omnion")
            .expect("a lazy pool is valid");
        let context = ActionContext {
            pool: &pool,
            organization_id: Uuid::nil(),
            site_id: None,
            execution_id: Uuid::nil(),
            step_id: Uuid::nil(),
            step_no: 1,
            attempt: 1,
        };

        let outcome = NoActionHandler
            .execute(
                "send_email",
                &serde_json::json!({ "to": "ada@example.com" }),
                &context,
            )
            .await;
        let message = outcome.expect_err("no handler, no success");
        assert!(message.contains("send_email"), "{message}");
        assert!(message.contains("installed none"), "{message}");
    }
}
