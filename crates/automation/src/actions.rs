//! The host actions this layer implements: `send_email`, `comment_revision`,
//! `http_request`, `publish_page` and `run_workflow`.
//!
//! The engine runs the synthetic actions itself and hands host actions over
//! (`omnion_workflows::Handler`), because they touch the world. This is that other side:
//!
//! * `send_email` — one plain-text message through the SMTP server of [`crate::mail`];
//! * `comment_revision` — one note on a content revision (`omnion_content::comments`);
//! * `http_request` — one signed outbound call, bounded by the host allow-list
//!   ([`crate::outbound`]);
//! * `publish_page` — one publication, bounded by the run's organization;
//! * `run_workflow` — one chained rule's run, bounded by the chain depth.
//!
//! A failure is a *message*, not an error type: the engine decides whether it means another
//! attempt (the retry policy) or a failed run, and the message is what an operator reads in the
//! step's `error` column. A step output is a small JSON object describing what happened, so a run
//! can be read back step by step.

use serde_json::{Value, json};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use omnion_content::comments::{self, CommentSource, NewRevisionComment};
use omnion_workflows::{ActionContext, ActionFuture, ActionHandler};

use crate::mail::{self, Email, MailSettings};
use crate::outbound::{self, HttpSettings};

/// The engine's host actions, bound to one process's database and mail server.
#[derive(Debug, Clone)]
pub struct AutomationActions {
    pool: PgPool,
    mail: MailSettings,
    http: HttpSettings,
}

impl AutomationActions {
    /// Bind the actions to a pool and a mail server.
    #[must_use]
    pub fn new(pool: PgPool, mail: MailSettings) -> Self {
        Self {
            pool,
            mail,
            http: HttpSettings::default(),
        }
    }

    /// The mail server these actions send through.
    #[must_use]
    pub fn mail(&self) -> &MailSettings {
        &self.mail
    }

    /// Set the outbound HTTP settings these actions call with.
    #[must_use]
    pub fn with_http(mut self, http: HttpSettings) -> Self {
        self.http = http;
        self
    }

    /// The outbound HTTP settings.
    #[must_use]
    pub fn http(&self) -> &HttpSettings {
        &self.http
    }
}

impl ActionHandler for AutomationActions {
    fn execute<'a>(
        &'a self,
        action: &'a str,
        params: &'a Value,
        context: &'a ActionContext<'_>,
    ) -> ActionFuture<'a> {
        Box::pin(async move {
            match action {
                "send_email" => self.send_email(params).await,
                "comment_revision" => self.comment_revision(params, context).await,
                "http_request" => self.http_request(params, context).await,
                "publish_page" => self.publish_page(params, context).await,
                "run_workflow" => self.run_workflow(params, context).await,
                other => Err(format!(
                    "`{other}` is not a host action of the automation layer"
                )),
            }
        })
    }
}

impl AutomationActions {
    /// Send one plain-text email.
    async fn send_email(&self, params: &Value) -> Result<Value, String> {
        let to = text(params, "to")?;
        let subject = text(params, "subject")?;
        let body = text(params, "body")?;

        let email = Email::new(to.clone(), subject.clone(), body);
        mail::send(&self.mail, &email)
            .await
            .map_err(|err| format!("the email could not be sent: {err}"))?;

        Ok(json!({
            "action": "send_email",
            "to": to,
            "subject": subject,
            "server": format!("{}:{}", self.mail.host, self.mail.port),
        }))
    }

    /// Write one comment on a revision.
    async fn comment_revision(
        &self,
        params: &Value,
        context: &ActionContext<'_>,
    ) -> Result<Value, String> {
        let revision_id = text(params, "revision_id")?;
        let revision_id = Uuid::parse_str(&revision_id)
            .map_err(|_| format!("`{revision_id}` is not a revision id"))?;
        let body = text(params, "body")?;

        let comment = comments::add(
            context.pool,
            NewRevisionComment {
                organization_id: context.organization_id,
                revision_id,
                author_user_id: None,
                source: CommentSource::Automation,
                body,
            },
        )
        .await
        .map_err(|err| format!("the comment could not be written: {err}"))?;

        Ok(json!({
            "action": "comment_revision",
            "comment_id": comment.id,
            "revision_id": comment.revision_id,
        }))
    }

    /// Make one signed outbound call, if the allow-list says the host may be reached.
    async fn http_request(
        &self,
        params: &Value,
        context: &ActionContext<'_>,
    ) -> Result<Value, String> {
        let workflow_id = self
            .workflow_of(context)
            .await
            .map_err(|err| err.to_string())?;

        outbound::http_request(
            &self.pool,
            params,
            &self.http,
            context.execution_id,
            workflow_id,
            OffsetDateTime::now_utc(),
        )
        .await
    }

    /// Publish one page of the run's own organization.
    async fn publish_page(
        &self,
        params: &Value,
        context: &ActionContext<'_>,
    ) -> Result<Value, String> {
        outbound::publish_page(&self.pool, params, context.organization_id, context.site_id).await
    }

    /// Start another rule's run as part of this one.
    async fn run_workflow(
        &self,
        params: &Value,
        context: &ActionContext<'_>,
    ) -> Result<Value, String> {
        let workflow_id = self
            .workflow_of(context)
            .await
            .map_err(|err| err.to_string())?;
        let depth = self.chain_depth(context).await.unwrap_or(1);

        outbound::run_workflow(
            &self.pool,
            params,
            context.organization_id,
            workflow_id,
            depth,
        )
        .await
    }

    /// The rule behind a run — the signer's identity, and the self-chain guard.
    async fn workflow_of(&self, context: &ActionContext<'_>) -> sqlx::Result<Uuid> {
        sqlx::query_scalar("select workflow_id from workflow_executions where id = $1")
            .bind(context.execution_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| sqlx::Error::RowNotFound)
    }

    /// How deep the current chain already is.
    ///
    /// A chained run carries its depth in the step that started it, so a run started by
    /// hand is depth 1 and each `run_workflow` adds one. The bound is what stops two rules
    /// that call each other from filling the queue.
    async fn chain_depth(&self, context: &ActionContext<'_>) -> sqlx::Result<usize> {
        let depth: Option<i32> = sqlx::query_scalar(
            "select (params ->> 'chain_depth')::int from workflow_steps \
             where execution_id = $1 and action = 'run_workflow' \
             order by step_no desc limit 1",
        )
        .bind(context.execution_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(usize::try_from(depth.unwrap_or(0)).unwrap_or(0) + 1)
    }
}

/// Read a resolved text parameter.
///
/// Parameters reach an action already resolved (placeholders were spliced in when the run was
/// materialised), so a missing or unusable one is a definition problem the engine has already
/// validated against — this is the belt-and-braces read for a row written by hand.
fn text(params: &Value, key: &str) -> Result<String, String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("the action needs a non-empty `{key}` parameter"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool_for_tests() -> PgPool {
        PgPool::connect_lazy("postgres://omnion@127.0.0.1/omnion").expect("lazy pool")
    }

    fn mail_settings() -> MailSettings {
        MailSettings::new("127.0.0.1", 1025, "omnion@localhost")
    }

    #[test]
    fn parameters_are_read_as_text() {
        assert_eq!(
            text(&json!({ "to": "ada@example.com" }), "to").expect("reads"),
            "ada@example.com"
        );
        assert!(text(&json!({}), "to").is_err());
        assert!(text(&json!({ "to": "  " }), "to").is_err());
        assert!(text(&json!({ "to": 7 }), "to").is_err());
    }

    #[tokio::test]
    async fn an_unknown_action_is_refused_rather_than_ignored() {
        // The handler is the last line of defence: the engine validates the catalogue, and a
        // handler still never pretends a step it does not know succeeded.
        let handler = AutomationActions::new(pool_for_tests(), mail_settings());
        let never = pool_for_tests();
        let context = ActionContext {
            pool: &never,
            organization_id: Uuid::nil(),
            site_id: None,
            execution_id: Uuid::nil(),
            step_id: Uuid::nil(),
            step_no: 1,
            attempt: 1,
        };

        // A real action name the *handler* does not implement: the engine's catalogue is
        // wider than one process's abilities, and a handler must say so rather than pretend.
        let message = handler
            .execute("smtp_send", &json!({}), &context)
            .await
            .expect_err("the handler does not know this action");
        assert!(message.contains("smtp_send"), "{message}");
        assert!(message.contains("not a host action"), "{message}");
    }

    #[tokio::test]
    async fn a_revision_id_that_is_not_one_fails_with_a_readable_message() {
        let handler = AutomationActions::new(pool_for_tests(), mail_settings());
        let never = pool_for_tests();
        let context = ActionContext {
            pool: &never,
            organization_id: Uuid::nil(),
            site_id: None,
            execution_id: Uuid::nil(),
            step_id: Uuid::nil(),
            step_no: 1,
            attempt: 1,
        };

        let message = handler
            .execute(
                "comment_revision",
                &json!({ "revision_id": "not-a-uuid", "body": "note" }),
                &context,
            )
            .await
            .expect_err("a malformed id cannot be a comment target");
        assert!(message.contains("not a revision id"), "{message}");
    }

    #[tokio::test]
    async fn an_email_with_an_unusable_recipient_fails_before_any_connection() {
        let handler = AutomationActions::new(pool_for_tests(), mail_settings());
        let never = pool_for_tests();
        let context = ActionContext {
            pool: &never,
            organization_id: Uuid::nil(),
            site_id: None,
            execution_id: Uuid::nil(),
            step_id: Uuid::nil(),
            step_no: 1,
            attempt: 1,
        };

        let message = handler
            .execute(
                "send_email",
                &json!({ "to": "not-an-address", "subject": "Hi", "body": "Hello" }),
                &context,
            )
            .await
            .expect_err("the recipient is refused");
        assert!(message.contains("could not be sent"), "{message}");
        assert!(message.contains("invalid address"), "{message}");
    }
}
