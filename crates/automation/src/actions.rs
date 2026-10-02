//! The host actions this layer implements: `send_email`, `comment_revision`,
//! `http_request`, `publish_page`, `run_workflow` and `ai.prompt`.
//!
//! The engine runs the synthetic actions itself and hands host actions over
//! (`omnion_workflows::Handler`), because they touch the world. This is that other side:
//!
//! * `send_email` — one plain-text message through the SMTP server of [`crate::mail`];
//! * `comment_revision` — one note on a content revision (`omnion_content::comments`);
//! * `http_request` — one signed outbound call, bounded by the host allow-list
//!   ([`crate::outbound`]);
//! * `publish_page` — one publication, bounded by the run's organization;
//! * `run_workflow` — one chained rule's run, bounded by the chain depth;
//! * `ai.prompt` — one prompt to a model, resolved through the AI Hub router.
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
use omnion_events::model::NewEvent;
use omnion_workflows::{ActionContext, ActionFuture, ActionHandler};

use crate::authority;
use crate::mail::{self, Email, MailSettings};
use crate::outbound::{self, HttpSettings};

/// Output budget an `ai.prompt` step uses when its definition does not name one.
///
/// Deliberately smaller than the registry's `MAX_AI_STEP_TOKENS`: a ceiling is what a
/// definition may ask for, and a default is what one normally gets. A step that says nothing
/// about its budget is a step whose author did not think about tokens, and the smaller number
/// is the one that is safe to spend by surprise. The limit is a platform fact, so it is read
/// from the engine's own constant rather than repeated here — two numbers that could differ
/// would mean a step silently running with a budget its definition was refused for.
const DEFAULT_AI_STEP_TOKENS: u32 = 1_000;

/// The largest budget a step may ask for, as the wire type.
const MAX_AI_STEP_TOKENS_U32: u32 = omnion_workflows::actions::MAX_AI_STEP_TOKENS as u32;

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
            // The authority check comes **first**, before any parameter is read and before
            // the world is touched. A refusal is not an action failure that a retry could
            // fix — the premise is false — so it is reported as a message the engine's
            // `stop` policy ends the run on, and the event is recorded so an operator can
            // see it without opening every run.
            if let Err(err) = self.authorise(context, action).await {
                self.record_revocation(context, action, &err).await;
                return Err(err.as_step_error());
            }

            match action {
                "send_email" => self.send_email(params).await,
                "comment_revision" => self.comment_revision(params, context).await,
                "http_request" => self.http_request(params, context).await,
                "publish_page" => self.publish_page(params, context).await,
                "run_workflow" => self.run_workflow(params, context).await,
                "ai.prompt" => self.ai_prompt(params).await,
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

    /// Send one prompt template to a model and put the answer in the step output.
    ///
    /// The model is resolved through the **AI Hub router** rather than a client of its own,
    /// so a rule's prompt is answered by the same provider, the same default and the same
    /// credential handling as the console's — two clients would be two answers to "which
    /// model serves this".
    ///
    /// The step output carries the answer under `text`, so a later step reads it as
    /// `{{steps.N.output.text}}` — the same shape every other action produces, which is what
    /// makes an AI step a normal step rather than a special case in the engine. The model
    /// key is echoed back because "which model answered this" is the first question an
    /// operator asks of a rule whose behaviour changed.
    async fn ai_prompt(&self, params: &Value) -> Result<Value, String> {
        let prompt = text(params, "prompt")?;
        let model = params
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let max_tokens = params
            .get("max_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(i64::from(DEFAULT_AI_STEP_TOKENS));
        // Clamped to the engine's ceiling, not merely converted: the registry refused a
        // definition above it when the rule was written, so reaching this point with a bigger
        // number means the definition was stored before the rule existed — and a step must
        // never spend more than the platform says a step may spend.
        let max_tokens = max_tokens.clamp(1, i64::from(MAX_AI_STEP_TOKENS_U32));

        let resolved = omnion_ai_hub::resolve(&self.pool, model)
            .await
            .map_err(|err| format!("no model could serve this step: {err}"))?;

        let outcome = omnion_ai_hub::chat(
            &omnion_ai_hub::ProviderTarget::from_provider(&resolved.provider),
            &omnion_ai_hub::ChatRequest {
                // The wire key, not the `provider/model` pair: the pair is the router's
                // vocabulary and the provider only knows its own key.
                model: resolved.model.model_key.clone(),
                messages: vec![omnion_ai_hub::ChatMessage {
                    role: omnion_ai_hub::ChatRole::User,
                    content: prompt.clone(),
                }],
                // No temperature, like every other schema-shaped call: a step that returns
                // prose does not need it to vary.
                temperature: None,
                // The engine already refused a budget above its ceiling at validation time,
                // so this clamp is the second belt and not the first.
                max_tokens: Some(u32::try_from(max_tokens).unwrap_or(DEFAULT_AI_STEP_TOKENS)),
            },
        )
        .await
        .map_err(|err| format!("the model did not answer this step: {err}"))?;

        if outcome.content.trim().is_empty() {
            return Err("the model answered with nothing, so the step produced no text".to_owned());
        }

        Ok(json!({
            "action": "ai.prompt",
            "text": outcome.content,
            "model": resolved.id(),
        }))
    }

    /// Check this step's action against the rule's authority.
    ///
    /// The account is read from the rule *at this moment* (REQ-003 slice 3), so a rule whose
    /// run-as account lost a permission stops on its next run rather than on the next
    /// deployment. An action with no permission of its own passes: the engine is the whole
    /// authority for something that touches only the run.
    async fn authorise(
        &self,
        context: &ActionContext<'_>,
        action: &str,
    ) -> std::result::Result<(), authority::Refusal> {
        if authority::permission_for(action).is_none() {
            return Ok(());
        }

        let authority = self.authority_of(context).await?;
        authority::authorise_action(&self.pool, &authority, context.organization_id, action).await
    }

    /// The rule's authority, read now rather than snapshotted.
    ///
    /// A missing rule is a refusal rather than a panic: the execution row outlives the
    /// rule when a rule is deleted mid-run, and a rule that is gone has no authority to
    /// act with.
    async fn authority_of(
        &self,
        context: &ActionContext<'_>,
    ) -> std::result::Result<authority::Authority, authority::Refusal> {
        let row: Option<(Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
            "select w.run_as_user_id, w.created_by from workflows w \
                 join workflow_executions e on e.workflow_id = w.id where e.id = $1",
        )
        .bind(context.execution_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| authority::Refusal {
            permission: "workflows.run",
            user_id: None,
            action: "this action",
            message: format!(
                "the run-as account could not be read, so the step was not run: {err}"
            ),
        })?;

        Ok(match row {
            Some((run_as, created_by)) => authority::Authority::of(run_as, created_by),
            None => authority::Authority::of(None, None),
        })
    }

    /// Record a `permission_revoked` refusal on the bus, best-effort.
    ///
    /// Best-effort on purpose: the refusal has already stopped the run and the step already
    /// carries the message, so a bus that is down must not turn a *refusal* into a retry —
    /// which is exactly what would happen if this were `?`. The warning is the signal that
    /// the notification did not go out.
    async fn record_revocation(
        &self,
        context: &ActionContext<'_>,
        action: &str,
        refusal: &authority::Refusal,
    ) {
        let emission = omnion_events::bus::emit(
            &self.pool,
            NewEvent::new(authority::PERMISSION_REVOKED_EVENT)
                .organization(context.organization_id)
                .site(context.site_id)
                .payload(refusal.as_event_payload()),
        )
        .await;

        match emission {
            Ok(report) => tracing::warn!(
                execution_id = %context.execution_id,
                step_no = context.step_no,
                action,
                permission = refusal.permission,
                event_id = report.event.id,
                "a rule lost the authority an action needed and its run stopped"
            ),
            Err(err) => tracing::warn!(
                execution_id = %context.execution_id,
                step_no = context.step_no,
                action,
                permission = refusal.permission,
                error = %err,
                "a rule lost the authority an action needed; the refusal could not be broadcast"
            ),
        }
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
    async fn the_authority_check_runs_before_any_parameter_is_read() {
        // The order is the feature: a step whose run-as account cannot even be read is
        // refused as `permission_revoked` and never reaches its own parameter validation.
        // A test that only checked "a malformed revision id is refused" would have passed
        // either way, and would not have noticed the two checks swapping places — which is
        // the swap that would let a rule with no authority read a target id first.
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
            .expect_err("no authority, no action");
        assert!(
            message.starts_with(crate::authority::PERMISSION_REVOKED_EVENT),
            "the refusal is reported as the event the request names: {message}"
        );
        assert!(!message.contains("not a revision id"), "{message}");
    }

    #[tokio::test]
    async fn an_action_with_no_permission_of_its_own_never_reaches_the_account() {
        // The synthetic actions are the engine's, and the engine is their whole authority.
        // Asking the database about a run-as account for an `echo` would be a query per step
        // for a step that cannot touch the world — and, worse, would make an echo fail on a
        // host whose database is briefly unreachable.
        assert_eq!(crate::authority::permission_for("echo"), None);

        // The handler refuses an action it does not implement, and it says so *without*
        // asking about authority first — an unknown action has no permission, and its
        // refusal is about the action's name, not about an account.
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
            .execute("smtp_send", &json!({}), &context)
            .await
            .expect_err("not a host action");
        assert!(message.contains("not a host action"), "{message}");
        assert!(
            !message.starts_with(crate::authority::PERMISSION_REVOKED_EVENT),
            "an unknown action is not a permission problem: {message}"
        );
    }

    #[test]
    fn the_ai_step_spends_tokens_and_so_rides_its_own_permission() {
        // The load-bearing decision of the slice: a rule that prompts a model on a schedule
        // costs money on every firing, so it must NOT inherit `workflows.run`. A role that may
        // start rules but may not chat cannot arm one.
        assert_eq!(crate::authority::permission_for("ai.prompt"), Some("ai.chat"));
        assert_ne!(
            crate::authority::permission_for("ai.prompt"),
            crate::authority::permission_for("send_email"),
            "an AI step and an email step spend different things, so they need different keys"
        );
    }

    #[test]
    fn the_ai_budget_is_read_from_the_engine_rather_than_repeated_here() {
        // The comment on both constants claims they cannot drift. This is the claim: the
        // ceiling this layer clamps to IS the ceiling the registry refuses a definition over,
        // so a step can never spend more than the engine said it may.
        assert_eq!(
            i64::from(MAX_AI_STEP_TOKENS_U32),
            omnion_workflows::actions::MAX_AI_STEP_TOKENS
        );
        // And the default is strictly inside the ceiling — a default above its own maximum
        // would be a step that spends the maximum by accident.
        assert!(DEFAULT_AI_STEP_TOKENS > 0);
        assert!(DEFAULT_AI_STEP_TOKENS < MAX_AI_STEP_TOKENS_U32);
    }
}
