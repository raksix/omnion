//! The host actions this layer implements: `send_email` and `comment_revision`.
//!
//! The engine runs the synthetic actions itself and hands host actions over
//! (`omnion_workflows::Handler`), because they touch the world. This is that other side:
//!
//! * `send_email` — one plain-text message through the SMTP server of [`crate::mail`];
//! * `comment_revision` — one note on a content revision (`omnion_content::comments`).
//!
//! A failure is a *message*, not an error type: the engine decides whether it means another
//! attempt (the retry policy) or a failed run, and the message is what an operator reads in the
//! step's `error` column. A step output is a small JSON object describing what happened, so a run
//! can be read back step by step.

use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use omnion_content::comments::{self, CommentSource, NewRevisionComment};
use omnion_workflows::{ActionContext, ActionFuture, ActionHandler};

use crate::mail::{self, Email, MailSettings};

/// The engine's host actions, bound to one process's database and mail server.
#[derive(Debug, Clone)]
pub struct AutomationActions {
    pool: PgPool,
    mail: MailSettings,
}

impl AutomationActions {
    /// Bind the actions to a pool and a mail server.
    #[must_use]
    pub fn new(pool: PgPool, mail: MailSettings) -> Self {
        Self { pool, mail }
    }

    /// The mail server these actions send through.
    #[must_use]
    pub fn mail(&self) -> &MailSettings {
        &self.mail
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
            &self.pool,
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

        let message = handler
            .execute("http_request", &json!({}), &context)
            .await
            .expect_err("the handler does not know this action");
        assert!(message.contains("http_request"), "{message}");
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
