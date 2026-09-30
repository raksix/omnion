//! One prompt, one answer, and the single repair round-trip (docs/requests/REQ-046).
//!
//! The request's risk section names the real hazard: *model output drift*. Three things keep
//! it from becoming a bad workflow, and this module is the middle one —
//! a closed action registry (the prompt), strict validation ([`crate::definition`]) and **one**
//! repair round-trip (this file). The third is exactly one, deliberately: a second attempt
//! spends a second round of tokens on a model that has already shown it cannot do this, and
//! the draft's error would then read like a platform failure rather than like an answer
//! nobody could produce.
//!
//! **A provider failure is not a bad answer.** Retrying a transport error produces an
//! identical answer, which the validator refuses identically, so the repair is spent on
//! nothing and the draft fails for a reason the operator cannot fix by asking again. Only a
//! *validation* failure is repairable, and only the validation failure is quoted back to the
//! model — with the exact message the engine produced, because that message already names the
//! actions that exist.

use omnion_ai_hub::client::{
    ChatMessage, ChatOutcome, ChatRequest, ChatRole, ProviderTarget, chat,
};
use omnion_ai_hub::ResolvedModel;
use serde_json::Value;

use crate::definition::{ParsedDefinition, parse_answer};
use crate::error::{AiWorkflowError, Result};
use crate::model::DraftTokens;

/// The one repair round-trip. Named so a caller cannot write "two" by accident.
pub const MAX_REPAIR_ROUNDS: usize = 1;

/// How many attempts one generation may make in total.
pub const MAX_ATTEMPTS: usize = MAX_REPAIR_ROUNDS + 1;

/// Output budget for the answer, in tokens.
///
/// A definition is small; a model that spends its budget on prose is the failure the repair
/// prompt's "nothing outside the object" rule addresses. The ceiling is the platform's, not
/// the model's: a request asking for more than the provider allows is refused by the AI Hub
/// before it is sent, so the number has to be one the registry admits.
pub const MAX_OUTPUT_TOKENS: u32 = 4096;

/// What one generation produced.
#[derive(Debug, Clone)]
pub struct GenerationOutcome {
    /// The title: the model's, or the caller's when it sent none.
    pub title: String,
    /// The rationale, when the model sent one.
    pub rationale: Option<String>,
    /// The validated definition.
    pub definition: Value,
    /// Which model answered, `provider/model`.
    pub model_key: String,
    /// What the whole generation cost, both attempts included.
    pub tokens: DraftTokens,
    /// How many provider calls this took (1, or 2 after a repair).
    pub attempts: usize,
    /// `true` when the first answer was refused and a repair was spent.
    pub repaired: bool,
}

/// The instruction the generation runs.
///
/// The action list is **built from the registry**, never typed out here. That is the load-bearing
/// decision of the whole request: a list written by hand drifts from
/// `omnion_workflows::actions`, and a model told about an action that was renamed or removed
/// produces a definition the engine refuses — spending the one repair round-trip on a problem
/// no repair can fix. The list is also what makes "a generated definition is an ordinary
/// definition" true in practice rather than only in the type system.
///
/// The template placeholders (`{{steps.1.output.x}}`) are the engine's own resolution
/// syntax, and the prompt shows one because a model that does not know them writes the
/// literal string into a parameter and the rule then sends `{{steps.1.output.x}}` to a
/// customer.
#[must_use]
pub fn system_prompt() -> String {
    let actions: Vec<String> = omnion_workflows::actions::keys()
        .into_iter()
        .map(|key| format!("- `{key}`"))
        .collect();

    format!(
        "You turn a plain-language description into one Omnion workflow definition.\n\
         \n\
         Answer with exactly one JSON object and nothing outside it:\n\
         {{\n  \"title\": \"a short name for the rule\",\n  \"rationale\": \
         \"one short paragraph explaining what you built and why\",\n  \"definition\": {{\n    \
         \"trigger\": {{ \"kind\": \"manual\" }},\n    \"steps\": [ {{ \"name\": \
         \"a unique step name\", \"kind\": \"task\", \"action\": \"<action>\", \"params\": {{}} }} ]\n  \
         }}\n}}\n\
         \n\
         The `definition` is the workflow the platform will run. Its rules are the engine's:\n\
         \n\
         - `trigger.kind` is `manual`, `schedule` or `event`.\n\
           A `schedule` needs a five-field cron in UTC: `\"cron\": \"0 9 * * *\"`.\n\
           An `event` needs a lower-case dotted event name: `\"event\": \"page.published\"`.\n\
           A `manual` trigger carries neither.\n\
         - `steps` is an ordered, non-empty list (at most 50) with unique names.\n\
         - a task step's `kind` is `task` and it names one `action` from the list below. \
         There is no other action: inventing one is refused.\n\
         - `params` is an object. Every key it carries must be a parameter that action \
         documents; a parameter you cannot fill in is better left out than guessed.\n\
         - `{{{{steps.1.output.customer.email}}}}` is how a later step reads an earlier step's \
         output. The number is the step's position counting from 1.\n\
         - **never put a credential in a definition.** No API keys, tokens or passwords — \
         the platform stores credentials itself and a definition is copied into logs, exports \
         and webhooks.\n\
         \n\
         The actions this platform has:\n{}\n",
        actions.join("\n")
    )
}

/// The user turn for one prompt.
///
/// `revision` carries an operator's "ask for changes" note, and `previous` the answer being
/// revised — the model is asked to *change* that definition rather than to answer afresh,
/// because an operator who wrote "only chase invoices in EUR" did not ask for a new rule.
#[must_use]
pub fn user_prompt(prompt: &str, revision: Option<&str>, previous: Option<&str>) -> String {
    let mut text = format!("Describe the workflow to build:\n\n{prompt}");
    if let Some(note) = revision.map(str::trim).filter(|note| !note.is_empty()) {
        text.push_str("\n\nChange this about it:");
        text.push_str("\n\n");
        text.push_str(note);
        if let Some(previous) = previous.map(str::trim).filter(|value| !value.is_empty()) {
            text.push_str("\n\nThis is what you answered last time — revise it, do not start over:");
            text.push_str("\n\n");
            text.push_str(previous);
        }
    }
    text
}

/// The repair turn: the answer that was refused, and the engine's own reason.
///
/// Both halves travel together because the second is meaningless without the first: "fix it"
/// is a refusal to act on, while the exact engine message is an instruction.
#[must_use]
pub fn repair_prompt(answer: &str, error: &AiWorkflowError) -> String {
    format!(
        "That answer was refused by the workflow engine.\n\n\
         Reason:\n\n{error}\n\n\
         Your answer:\n\n{answer}\n\n\
         Answer again with the same JSON shape, changed only where the reason says. \
         Nothing outside the JSON object.",
        error = error.to_string(),
    )
}

/// How a generation wants its provider calls made.
///
/// Split out so the round-trip is testable without a provider: everything below this struct is
/// a pure decision, and only this half touches the network.
pub struct GenerationPlan {
    /// The provider to call.
    pub target: ProviderTarget,
    /// The wire key of the model.
    pub model: String,
    /// `provider/model`, for the draft's frozen `model_key`.
    pub model_id: String,
    /// The system turn, built once.
    pub system: String,
}

/// The request for one attempt.
///
/// `transcript` is what came before: empty on a first generation, the answer and the engine's
/// reason on a repair.
#[must_use]
pub fn request_for(
    plan: &GenerationPlan,
    user: &str,
    transcript: &[ChatMessage],
) -> ChatRequest {
    let mut messages = Vec::with_capacity(transcript.len() + 2);
    messages.push(ChatMessage {
        role: ChatRole::System,
        content: plan.system.clone(),
    });
    messages.push(ChatMessage {
        role: ChatRole::User,
        content: user.to_owned(),
    });
    messages.extend_from_slice(transcript);

    ChatRequest {
        model: plan.model.clone(),
        messages,
        // No temperature: the request is a *schema*, and a schema asked for at 0.7 comes back
        // half the time with a field the engine did not ask for. The cost of the single repair
        // round-trip is paid by asking for the same thing twice, not by asking differently.
        temperature: None,
        max_tokens: Some(MAX_OUTPUT_TOKENS),
    }
}

/// How one attempt turned out.
#[derive(Debug)]
pub enum Attempt {
    /// A provider answer that parsed and validated.
    Answered(ParsedDefinition, ChatOutcome),
    /// An answer the engine refused; repairable, and the reason travels into the next turn.
    Refused(String, AiWorkflowError),
}

/// Fold one attempt into the run, and say whether another one is owed.
///
/// Pure, because the *policy* is the claim the request makes ("exactly one repair
/// round-trip") and a policy that can only be measured by counting provider calls is not
/// measured at all. Three states, and the fourth is the bug this shape exists to prevent:
/// repair a **provider** failure (the second call would fail identically and the draft would
/// end `failed` for a reason "ask again" cannot fix), repair twice, or report `done` when
/// nothing has been answered.
///
/// `history` is the text of the first refused answer, or `None` on the first attempt. It is
/// the answer and **not** the attempt on purpose: a second refusal's message quotes the first
/// answer, and taking the whole `Attempt` to read one `String` out of it would force every
/// caller to clone an error that holds a `sqlx::Error` — which is not `Clone` and should not
/// be made so for a log line.
#[must_use]
pub fn fold(history: Option<&str>, attempt: Attempt, round_index: usize) -> Fold {
    match attempt {
        Attempt::Answered(parsed, outcome) => Fold::Done(parsed, outcome),
        Attempt::Refused(answer, error) => {
            // A PROVIDER failure is not repairable, and this is the only place that knows it:
            // a transport error reproduced with the identical request fails identically, so a
            // "repair" would spend a second round of tokens and end the draft with a reason
            // "ask again" cannot fix. The operator's remedy is a different model, not a
            // second call.
            if error.is_provider_failure() {
                return Fold::Failed(error.to_string());
            }
            match history {
                Some(first) => Fold::Failed(format!(
                    "the answer was refused twice. First: {first}. After the repair: {error}"
                )),
                None if round_index < MAX_ATTEMPTS => Fold::Repair { answer, error },
                None => Fold::Failed(error.to_string()),
            }
        }
    }
}

/// What the caller should do next.
#[derive(Debug)]
pub enum Fold {
    /// The generation has an answer.
    Done(ParsedDefinition, ChatOutcome),
    /// Spend the repair round-trip: this answer and the reason travel into the next turn.
    Repair {
        /// The answer that was refused, sent back so the model revises rather than restarts.
        answer: String,
        /// The engine's own message.
        error: AiWorkflowError,
    },
    /// Nothing more is owed; the string is what the draft's `error` column carries.
    Failed(String),
}

/// Where a generation wants to happen and against which model.
#[derive(Debug, Clone)]
pub struct GenerationRequest {
    /// `provider/model`, or `None` for the installation's default.
    pub model: Option<String>,
    /// The operator's sentence.
    pub prompt: String,
    /// The operator's "ask for changes" note, when revising.
    pub revision: Option<String>,
    /// The definition being revised, serialised, when revising.
    pub previous: Option<String>,
}

/// Address one generation's provider calls.
///
/// The only place a resolved pair becomes a wire target, and the reason [`GenerationPlan`]
/// exists separately: everything above this function is a pure decision that a unit test can
/// reach, and everything below it is a network call a unit test cannot.
pub fn plan_for(resolved: &ResolvedModel) -> GenerationPlan {
    GenerationPlan {
        target: ProviderTarget::from_provider(&resolved.provider),
        model: resolved.model.model_key.clone(),
        model_id: resolved.id(),
        system: system_prompt(),
    }
}

/// Run one generation to a decision, spending at most [`MAX_ATTEMPTS`] provider calls.
///
/// The loop is written around [`fold`] rather than around the counter, because the policy the
/// request states — *exactly one repair round-trip, and never one for a provider failure* — is
/// a property of the fold. A loop that counted calls itself would enforce a different and
/// weaker rule: it would repair a timeout, spending a second round of tokens to learn exactly
/// what the first one already said.
///
/// The resolved model is a **parameter** rather than a query here, because the console has
/// already resolved the same pair to render its model picker, and resolving twice would make
/// two queries disagree about which model is the default. The returned error is the draft's:
/// the caller stores it in `error` and leaves the row in `failed`.
pub async fn generate(
    resolved: &ResolvedModel,
    request: &GenerationRequest,
) -> Result<GenerationOutcome> {
    let plan = plan_for(resolved);
    let user = user_prompt(
        &request.prompt,
        request.revision.as_deref(),
        request.previous.as_deref(),
    );

    let mut tokens = DraftTokens::default();
    let mut transcript: Vec<ChatMessage> = Vec::new();
    let mut refused: Option<String> = None;
    let mut turn = user;

    for round in 0..MAX_ATTEMPTS {
        let answer = match chat(&plan.target, &request_for(&plan, &turn, &transcript)).await {
            Ok(outcome) => outcome,
            // A transport failure is the provider's, so it is classified as such: it ends the
            // generation at once and never costs a repair round-trip.
            Err(error) => return Err(ai_hub_error(error)),
        };
        // A usage block that reports no count for a field is `None`, not `0`: "did not report"
        // and "reported nothing" are different facts, and only the second may be stored as 0.
        // The cast **saturates**: a provider reporting a `u64` count wider than an `i64` must
        // not wrap into a negative number, which would then be stored in a column with a
        // `>= 0` check and turn the generation into a constraint violation.
        tokens.add(
            answer
                .usage
                .as_ref()
                .and_then(|usage| usage.prompt_tokens)
                .map(|count| i64::try_from(count).unwrap_or(i64::MAX)),
            answer
                .usage
                .as_ref()
                .and_then(|usage| usage.completion_tokens)
                .map(|count| i64::try_from(count).unwrap_or(i64::MAX)),
        );

        let attempt = match parse_answer(&answer.content) {
            Ok(parsed) => Attempt::Answered(parsed, answer),
            Err(error) => Attempt::Refused(answer.content.clone(), error),
        };

        match fold(refused.as_deref(), attempt, round) {
            Fold::Done(parsed, _) => {
                return Ok(GenerationOutcome {
                    title: parsed.title,
                    rationale: parsed.rationale,
                    definition: parsed.definition,
                    model_key: plan.model_id,
                    tokens,
                    attempts: round + 1,
                    repaired: round > 0,
                });
            }
            Fold::Repair { answer, error } => {
                refused = Some(answer.clone());
                // The refused answer and the engine's reason travel as history, so the model
                // revises the answer rather than answering the prompt again from scratch.
                // The reason itself is quoted with the answer: "that answer was refused, fix
                // it" is a refusal to act on, and an empty "Your answer:" section is the same
                // refusal with a blank where the evidence was.
                transcript.push(ChatMessage {
                    role: ChatRole::Assistant,
                    content: answer.clone(),
                });
                transcript.push(ChatMessage {
                    role: ChatRole::User,
                    content: repair_prompt(&answer, &error),
                });
                turn = String::new();
            }
            Fold::Failed(message) => {
                return Err(AiWorkflowError::invalid("generation_failed", message));
            }
        }
    }

    // Named rather than `unreachable!()`: a loop that exited because the budget ran out has
    // said nothing true about the draft's state, and the row must still be readable.
    Err(AiWorkflowError::invalid(
        "generation_failed",
        "the generation used its attempt budget without an answer",
    ))
}

/// Turn an AI Hub failure into this module's error, keeping its message.
///
/// The Hub's own text is the operator's clue ("no default model is configured", "the provider
/// did not answer") and translating it into a generic string would throw that away; the
/// classification is the thing that matters, and [`AiWorkflowError::Ai`] is what marks a
/// failure as the provider's rather than the draft's.
fn ai_hub_error(error: omnion_ai_hub::AiHubError) -> AiWorkflowError {
    AiWorkflowError::Ai(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refusal(code: &'static str) -> AiWorkflowError {
        AiWorkflowError::invalid(code, "the engine said no")
    }

    /// The provider half, faked. Nothing below talks to a network, which is what makes the
    /// "exactly one repair" claim a test rather than a hope.
    ///
    /// A scripted answer is `(text, outcome)`: the **text** always exists, because the second
    /// refusal's message quotes the first answer verbatim, and the **outcome** says whether the
    /// engine accepted it. Splitting the two is what makes the tripwire honest — a fixture that
    /// only carried a `Result` would have no way to assert *what* was refused, and the test
    /// would end up asserting on a hard-coded literal that no longer matches the prompt.
    ///
    /// The queue is popped, never indexed: a third call panics on an empty queue instead of
    /// silently reusing an answer, which is the whole "exactly one repair" claim as a test.
    struct Fake {
        calls: Vec<String>,
        scripted: std::collections::VecDeque<(String, std::result::Result<(), AiWorkflowError>)>,
    }

    impl Fake {
        /// A provider that answers `answers` in order.
        fn answering(answers: Vec<std::result::Result<String, AiWorkflowError>>) -> Self {
            Self {
                calls: Vec::new(),
                scripted: answers
                    .into_iter()
                    .map(|answer| match answer {
                        Ok(text) => (text, Ok(())),
                        Err(error) => ("refused answer".to_owned(), Err(error)),
                    })
                    .collect(),
            }
        }

        /// One provider call.
        fn call(&mut self) -> Attempt {
            let (text, outcome) = self.scripted.pop_front().unwrap_or_else(|| {
                panic!(
                    "the fake ran out of answers after {} call(s): a third call means the \
                     one-repair budget was not enforced",
                    self.calls.len()
                )
            });
            match outcome {
                Ok(()) => {
                    self.calls.push(text.clone());
                    let parsed = parse_answer(&text).expect("the fake's accepted answers validate");
                    Attempt::Answered(
                        parsed,
                        ChatOutcome {
                            content: text,
                            finish_reason: None,
                            usage: None,
                        },
                    )
                }
                Err(error) => Attempt::Refused(text, error),
            }
        }
    }

    #[test]
    fn the_prompt_carries_the_registry_not_a_hand_written_list() {
        let prompt = system_prompt();
        // Every action the engine knows must be named, or the model will invent one and the
        // single repair round-trip will be spent on a list the prompt itself got wrong.
        for key in omnion_workflows::actions::keys() {
            assert!(prompt.contains(&format!("`{key}`")), "the prompt omits `{key}`");
        }
        // And the things that make an answer usable: the shape, the trigger kinds, the
        // template syntax, the no-credentials rule.
        for needle in [
            "\"trigger\"",
            "\"steps\"",
            "manual",
            "schedule",
            "event",
            "{{steps.1.output.customer.email}}",
            "never put a credential",
        ] {
            assert!(prompt.contains(needle), "the prompt omits {needle:?}");
        }
    }

    #[test]
    fn the_prompt_would_not_survive_a_renamed_action() {
        // The failure this guards: someone renames `noop` in the engine and forgets the
        // prompt. The prompt is built from the registry, so it cannot go stale — and this test
        // is what proves the wiring, since a hardcoded list would pass every other test here.
        let keys = omnion_workflows::actions::keys();
        assert!(keys.contains(&"noop"));
        assert!(system_prompt().contains("`noop`"));
        // If a key were invented here it would not be in the registry, and this would fail:
        assert!(!system_prompt().contains("`smtp.send`"));
    }

    #[test]
    fn a_revision_carries_the_previous_answer_so_the_model_revises_rather_than_restarts() {
        let text = user_prompt(
            "chase overdue invoices",
            Some("only invoices in EUR"),
            Some(r#"{"definition":{}}"#),
        );
        assert!(text.contains("only invoices in EUR"));
        assert!(text.contains(r#"{"definition":{}}"#));
        assert!(text.contains("revise it, do not start over"));
    }

    #[test]
    fn a_first_turn_carries_no_revision_and_no_previous() {
        let text = user_prompt("chase overdue invoices", None, None);
        assert_eq!(text, "Describe the workflow to build:\n\nchase overdue invoices");
        assert!(!text.contains("start over"));
    }

    #[test]
    fn the_repair_prompt_quotes_the_engines_own_reason_with_the_answer() {
        let error = refusal("invalid_step_action");
        let text = repair_prompt("{\"definition\": {}}", &error);
        // Both halves, or the model is asked to fix something it cannot see.
        assert!(text.contains("the engine said no"), "{text}");
        assert!(text.contains(r#"{"definition": {}}"#), "{text}");
        assert!(
            text.to_lowercase().contains("nothing outside the json object"),
            "{text}"
        );
    }

    #[test]
    fn a_repair_prompt_carries_a_non_empty_answer_and_never_a_blank_evidence_line() {
        // The failure this guards is the one the driver can make silently: it holds the refused
        // text in two places, and passing the wrong one to `repair_prompt` compiles, passes
        // every other test, and ships a model a prompt whose "Your answer:" section is empty —
        // a repair request with the evidence removed. Asserting on the *rendered* section, not
        // on the arguments, is what makes it a test of the prompt rather than of the signature.
        let error = refusal("invalid_step_action");
        let text = repair_prompt(r#"{"definition": {"steps": []}}"#, &error);
        let section = text
            .split("Your answer:")
            .nth(1)
            .expect("the prompt has an answer section")
            .trim();
        assert!(!section.is_empty(), "the evidence section is blank: {text}");
        assert!(section.contains("steps"), "{text}");

        // And the inverse, asserted the way it is actually observable: a whitespace answer
        // leaves a *gap* in the section — the trailing instruction still follows it, so
        // "is the section empty" is the wrong question. What must never happen is a section
        // that carries the instruction but none of the evidence, so the check is on the
        // evidence's own presence between the two markers.
        let empty = repair_prompt("   ", &error);
        let section = empty
            .split("Your answer:")
            .nth(1)
            .expect("the prompt has an answer section");
        assert!(
            !section.contains("steps"),
            "a whitespace answer carries no evidence, which is why the driver passes the real \
             text rather than a placeholder"
        );
    }

    #[test]
    fn exactly_one_repair_round_trip_is_spent_and_then_the_draft_fails() {
        // The claim the request makes, measured without a provider.
        let mut fake = Fake::answering(vec![
            Err(refusal("nope")),
            Err(refusal("nope")),
            // A third entry the loop must never reach: if the budget is enforced, this stays
            // in the queue and the test passes; if it is not, `call()` panics naming the fact.
            Ok(valid_answer()),
        ]);

        // Attempt 1: refused → one repair owed.
        let first = fold(None, fake.call(), 0);
        let Fold::Repair { answer, error } = first else {
            panic!("the first refusal must be repairable, got {first:?}");
        };
        assert_eq!(answer, "refused answer");
        assert_eq!(error.code(), "nope");

        // Attempt 2: repaired into a refusal → failed, with both reasons. Only the first
        // answer's text is carried forward, which is all the second message quotes.
        let second = fold(Some(&answer), fake.call(), 1);
        let Fold::Failed(message) = &second else {
            panic!("a second refusal must end the draft, got {second:?}");
        };
        // The message names BOTH: "the repair did not help" is the question an operator asks,
        // and a message carrying only the second reason hides that the first was repairable.
        assert!(message.contains("refused twice"), "{message}");
        assert!(message.contains("the engine said no"), "{message}");
    }

    #[test]
    fn a_first_answer_that_validates_never_repairs() {
        let mut fake = Fake::answering(vec![Ok(valid_answer()), Ok(valid_answer())]);
        let outcome = fold(None, fake.call(), 0);
        assert!(matches!(outcome, Fold::Done(..)), "a good answer is spent once");
        assert_eq!(fake.calls.len(), 1, "and no second provider call was invented");
    }

    #[test]
    fn a_provider_failure_is_not_repairable() {
        // The distinction the whole file's policy turns on: a transport error reproduced with
        // the identical request would be refused identically, so a "repair" would spend a
        // second round of tokens and fail the draft for a reason nobody can act on.
        let transport = AiWorkflowError::Ai("the AI provider did not answer: timeout".to_owned());
        assert!(transport.is_provider_failure());

        // It is reported as `Refused` so the arm that folds it sees the failure rather than
        // re-querying, and the caller sees the provider's own message.
        let folded = fold(
            None,
            Attempt::Refused("no answer".to_owned(), transport),
            0,
        );
        let Fold::Failed(message) = folded else {
            panic!("a provider failure ends the generation at once");
        };
        assert!(message.contains("did not answer"), "{message}");
    }

    #[test]
    fn a_validation_failure_is_repairable_and_a_provider_failure_is_not() {
        assert!(!refusal("invalid_step_action").is_provider_failure());
        assert!(AiWorkflowError::Ai("x".to_owned()).is_provider_failure());
    }

    #[test]
    fn the_generation_attempt_budget_is_one_plus_one() {
        // Written as a constant rather than as arithmetic at the call site so a "let me try
        // three times" edit has to change this test.
        assert_eq!(MAX_REPAIR_ROUNDS, 1);
        assert_eq!(MAX_ATTEMPTS, 2);
    }

    #[test]
    fn the_request_is_a_schema_so_it_asks_for_no_temperature() {
        let plan = GenerationPlan {
            target: ProviderTarget {
                id: uuid::Uuid::nil(),
                name: "test".to_owned(),
                protocol: "openai_compatible".to_owned(),
                base_url: "http://127.0.0.1:1/v1".to_owned(),
                api_key: None,
            },
            model: "test-model".to_owned(),
            model_id: "test/test-model".to_owned(),
            system: "system".to_owned(),
        };
        let request = request_for(&plan, "build me a rule", &[]);
        assert_eq!(request.messages.len(), 2);
        assert_eq!(request.messages[0].role, ChatRole::System);
        assert_eq!(request.messages[1].role, ChatRole::User);
        assert_eq!(request.temperature, None, "a schema is not a creative task");
        assert_eq!(request.max_tokens, Some(MAX_OUTPUT_TOKENS));
    }

    #[test]
    fn a_repair_request_carries_the_failed_turns_before_the_new_instruction() {
        let plan = GenerationPlan {
            target: ProviderTarget {
                id: uuid::Uuid::nil(),
                name: "test".to_owned(),
                protocol: "openai_compatible".to_owned(),
                base_url: "http://127.0.0.1:1/v1".to_owned(),
                api_key: None,
            },
            model: "test-model".to_owned(),
            model_id: "test/test-model".to_owned(),
            system: "system".to_owned(),
        };
        let transcript = [
            ChatMessage { role: ChatRole::User, content: "first".to_owned() },
            ChatMessage { role: ChatRole::Assistant, content: "bad answer".to_owned() },
            ChatMessage { role: ChatRole::User, content: "that was refused".to_owned() },
        ];
        let request = request_for(&plan, "second try", &transcript);
        // System first, then the new instruction, then the history — the reverse order would
        // read as a new conversation with the failure quoted at the reader, not to the model.
        assert_eq!(request.messages.len(), 5);
        assert_eq!(request.messages[0].role, ChatRole::System);
        assert_eq!(request.messages[1].content, "second try");
        assert_eq!(request.messages[2].content, "first");
        assert_eq!(request.messages[4].content, "that was refused");
    }

    fn valid_answer() -> String {
        r#"{"title": "T","definition": {"trigger": {"kind": "manual"},
           "steps": [{"name": "x", "kind": "task", "action": "noop", "params": {}}]}}"#
            .to_owned()
    }
}
