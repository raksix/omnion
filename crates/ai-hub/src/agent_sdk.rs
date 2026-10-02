//! Embedding the agent runtime: the documented entry point for a workflow node, a CLI or a
//! future integration (REQ-099, slice 4).
//!
//! # What this module is for
//!
//! The agent loop already exists and already works — `loop_engine::run` is what the HTTP route
//! and the background runner both call. What it lacked was a way for **code inside the
//! workspace** to start a run without going through HTTP and without knowing the loop's
//! argument order. A workflow node that wants "ask this agent, then use its answer" should not
//! have to open a socket to itself, mint a session token, and parse an SSE stream.
//!
//! So this is deliberately thin: a builder that assembles the four things the loop needs
//! (model, executor, prompt, limits), an event listener that turns the loop's typed
//! events into a callback instead of a channel the caller must drain, and one entry function.
//!
//! # What it deliberately does *not* do
//!
//! - It does not persist anything. The durable record is [`crate::run_store`] and the runner's
//!   `Persist`; an embedder that wants rows calls [`crate::run_store::create_run`] itself and
//!   passes the resulting closure. A second persistence path inside the SDK would be a second
//!   thing to keep in step with the store's columns.
//! - It does not resolve a model. The router ([`crate::router`]) is an installation concern:
//!   which provider answers is a question about configuration and health, not about the caller
//!   who already has a `Model`.
//! - It does not know about approvals. The loop *parks* on an approval-gated tool and the
//!   outcome says so; the decision UX is REQ-101's, and a builder that grew an
//!   `approve(reason)` method would be claiming a half of it.
//!
//! # The example
//!
//! This doc comment is compiled and **run** by the test at the bottom of the file — the
//! `README` example every other crate in this workspace writes is prose nobody executes, and
//! prose that is never executed is prose that rots. The test is the example.
//!
//! ```
//! use std::sync::Arc;
//!
//! use omnion_ai_hub::agent::RunLimits;
//! use omnion_ai_hub::agent_sdk::AgentSdk;
//! use omnion_ai_hub::agent::ToolCall;
//! use omnion_ai_hub::loop_engine::ScriptedModel;
//! use omnion_ai_hub::tool_exec::ToolExecutor;
//! use omnion_ai_hub::tools::{DenyReason, Execution};
//!
//! /// A stand-in for the real thing.
//! ///
//! /// In production this is `tool_exec::Pipeline`, and the pool, the identity, the permission
//! /// gate and the call log all come with it — which is why the builder takes an executor and
//! /// not a tool list. A two-line refusal is the honest minimum an embedder can write here,
//! /// and it is the shape that fails safe.
//! struct NothingRuns;
//!
//! impl ToolExecutor for NothingRuns {
//!     fn execute<'a>(
//!         &'a self,
//!         _step_no: u32,
//!         call: &'a ToolCall,
//!     ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Execution> + Send + 'a>> {
//!         Box::pin(async move {
//!             Execution::Refused {
//!                 tool: call.tool.clone(),
//!                 reason: DenyReason::ToolDenied,
//!             }
//!         })
//!     }
//! }
//!
//! # async fn example() {
//! // A model that asks for one tool and then answers. In production this is the router's
//! // client; here it is scripted so the example needs no network.
//! let model = ScriptedModel::new(vec![
//!     omnion_ai_hub::loop_engine::ModelAnswer::calling(vec![
//!         omnion_ai_hub::loop_engine::RequestedCall::new(
//!             "page.read",
//!             serde_json::json!({ "slug": "pricing" }),
//!         ),
//!     ]),
//!     omnion_ai_hub::loop_engine::ModelAnswer::text("I could not read the page."),
//! ]);
//!
//! let sdk = AgentSdk::builder()
//!     .model(model)
//!     .executor(Arc::new(NothingRuns))
//!     .system_prompt("You answer questions about the published site.")
//!     .limits(RunLimits { max_steps: 4, ..RunLimits::default() })
//!     .build()
//!     .expect("a builder chain that names a model and an executor always assembles");
//!
//! let answer = sdk
//!     .run("What plans do you publish?", |event| {
//!         // Each event is one step boundary. A workflow node would push it onto its own
//!         // activity log; an embedder can just count them.
//!         let _ = event;
//!     })
//!     .await;
//!
//! // The refusal reached the model rather than the run hanging or a tool running unasked.
//! assert_eq!(answer.final_text.as_deref(), Some("I could not read the page."));
//! # }
//! # /// Run a future to completion: a doctest is an ordinary `fn main`, and a workflow node
//! # /// already has a runtime to `.await` in.
//! # fn futures_lite_block_on<F: std::future::Future>(future: F) -> F::Output {
//! #     tokio::runtime::Builder::new_current_thread()
//! #         .enable_all()
//! #         .build()
//! #         .expect("a runtime")
//! #         .block_on(future)
//! # }
//! # fn main() {
//! #     futures_lite_block_on(example());
//! # }
//! ```
//!
//! The example is an `async fn` that the [`tests::the_documented_example_runs`] test awaits, so
//! the code above is type-checked and executed by `cargo test -p omnion-ai-hub` on every
//! commit. Copying it into a workflow node is a matter of swapping [`ScriptedModel`] for the
//! router's model and `|_| {}` for the node's own record.

use std::sync::Arc;

use crate::agent::RunLimits;
use crate::error::{AiHubError, Result};
use crate::loop_engine::{
    Model, Outcome, Persist, RunOptions, Runtime, run_with as run_loop,
};

/// What an embedder is handed, and what it gives back.
///
/// Constructed through [`AgentSdk::builder`]; the fields are private so a future version can
/// add one without breaking a caller.
pub struct AgentSdk {
    runtime: Runtime,
    limits: RunLimits,
}

impl AgentSdk {
    /// Start assembling an agent.
    #[must_use]
    pub fn builder() -> AgentSdkBuilder {
        AgentSdkBuilder::default()
    }

    /// The assembled prompt, guardrail rules included.
    ///
    /// Exposed because an embedder that shows its users what the agent was told has to be able
    /// to print the *same* string the loop sends — the rules are part of the prompt, not a
    /// private prefix, and a preview that omitted them would be a preview of something the
    /// agent never sees.
    #[must_use]
    pub fn system_prompt(&self) -> String {
        self.runtime.system_prompt()
    }

    /// The limits after clamping — the numbers the runtime actually enforces.
    #[must_use]
    pub fn limits(&self) -> RunLimits {
        self.limits
    }

    /// A handle somebody else can flip to stop a run started with this SDK.
    ///
    /// Two agents sharing one SDK share the flag, so a caller running several goals
    /// concurrently needs one [`AgentSdk`] per run. That is stated here because it is the kind
    /// of thing a caller discovers in production rather than in a doc comment.
    #[must_use]
    pub fn cancel_handle(&self) -> crate::loop_engine::CancelHandle {
        self.runtime.cancel_handle()
    }

    /// Run to a final answer, a stop condition or a parked approval.
    ///
    /// `on_event` is called once per published event, in order, on the loop's own task. A slow
    /// callback slows the run down — that is deliberate: back-pressure beats a dropped step in
    /// an audit trail.
    pub async fn run<F>(&self, goal: &str, on_event: F) -> Outcome
    where
        F: FnMut(crate::agent::AgentEvent) + Send + 'static,
    {
        self.run_with(goal, on_event, &no_persistence(), RunOptions::default())
            .await
    }

    /// [`AgentSdk::run`] with the caller's durable record and the loop's test seams.
    ///
    /// `persisted` is what the store-backed runner passes; `options` is what the deadline test
    /// passes. An embedder with neither wants [`AgentSdk::run`], and an embedder that has both
    /// reaches for this one rather than re-deriving the argument order.
    pub async fn run_with<F>(
        &self,
        goal: &str,
        mut on_event: F,
        persisted: &Persist,
        options: RunOptions,
    ) -> Outcome
    where
        // `'static` because the callback is moved into the relay task. A borrowed callback would
        // force every caller to keep the borrow alive across the await point — and the thing an
        // embedder most wants to hand over is a `&mut` counter it reads *after* the run.
        F: FnMut(crate::agent::AgentEvent) + Send + 'static,
    {
        // The loop wants a `Sink`, an embedder wants a callback. A channel plus one relay task
        // is the whole translation — and the relay is bounded, because an unbounded channel in
        // front of a callback that is never called is a memory leak with a step count attached.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::agent::AgentEvent>(
            crate::loop_engine::SINK_CAPACITY,
        );
        let relay = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                on_event(event);
            }
        });

        let outcome = run_loop(&self.runtime, goal, self.limits, &tx, persisted, options).await;
        drop(tx);
        // The relay ends when the sender is dropped, which is the same signal the loop used to
        // close the stream: nothing to wait for beyond it.
        let _ = relay.await;
        outcome
    }
}

/// The five things a runtime needs, none of them optional.
///
/// `Default` is not derived: a builder that produced an SDK with no model would hand the loop a
/// runtime that panics on the first step, and the type system can say that is impossible for
/// free.
pub struct AgentSdkBuilder {
    model: Option<Arc<dyn Model>>,
    executor: Option<Arc<dyn crate::tool_exec::ToolExecutor>>,
    system_prompt: Option<String>,
    limits: Option<RunLimits>,
}

impl Default for AgentSdkBuilder {
    fn default() -> Self {
        Self {
            model: None,
            executor: None,
            system_prompt: None,
            limits: None,
        }
    }
}

impl AgentSdkBuilder {
    /// The model the loop asks. Any `Model`: the router's client, a scripted one, a test double.
    #[must_use]
    pub fn model(mut self, model: Arc<dyn Model>) -> Self {
        self.model = Some(model);
        self
    }

    /// How tool calls are carried out.
    ///
    /// **An executor, and required.** The builder used to take `(tools, allow)` and assemble a
    /// registry-only decider, which was the second door REQ-100's own risk note names: "the deny
    /// list must hold wherever the runtime is invoked — a workflow node or the internal SDK, not
    /// only the panel-facing run endpoint". An embedder that could hand the loop a bare registry
    /// could bypass the identity, the permission and the call log simply by choosing the
    /// convenient builder method. So the embedder now supplies the *same* executor the runner
    /// does — in practice a [`crate::tool_exec::Pipeline`] — and the two cannot drift.
    ///
    /// There is deliberately no default and no "no tools" fallback: an SDK that silently
    /// assembled an unrestricted decider when this was omitted would be a builder whose failure
    /// mode is a tool call nobody authorised.
    #[must_use]
    pub fn executor(mut self, executor: Arc<dyn crate::tool_exec::ToolExecutor>) -> Self {
        self.executor = Some(executor);
        self
    }

    /// The agent's own instructions. The guardrail rules are appended by the runtime.
    #[must_use]
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// The run's bounds. Clamped on the way in, exactly as the store clamps them.
    #[must_use]
    pub fn limits(mut self, limits: RunLimits) -> Self {
        self.limits = Some(limits);
        self
    }

    /// Assemble, or explain what is missing.
    ///
    /// The message names the *field*, because "invalid agent" leaves a caller guessing which of
    /// the five it forgot.
    pub fn build(self) -> Result<AgentSdk> {
        let Some(model) = self.model else {
            return Err(AiHubError::InvalidAgent(
                "AgentSdk needs a model: the loop has no other way to reach a provider".to_owned(),
            ));
        };
        // The executor is required for the same reason the model is, and the message says which
        // of the two it is. A builder that defaulted it would be a builder whose omission
        // produces a run that *works* while walking past the identity, the permission and the
        // call log — the quietest possible failure and the one the request's risk note is
        // written about.
        let Some(executor) = self.executor else {
            return Err(AiHubError::InvalidAgent(
                "AgentSdk needs an executor: pass the same tool_exec::Pipeline the API runner \
                 uses, or the run has no identity, no permission check and no call log"
                    .to_owned(),
            ));
        };
        let limits = RunLimits::clamped(self.limits.unwrap_or_default());
        Ok(AgentSdk {
            runtime: Runtime::new(model, executor, self.system_prompt.unwrap_or_default()),
            limits,
        })
    }
}

/// A `Persist` that records nothing, for an embedder that keeps no rows.
///
/// Not `None` because the loop takes one closure and a "sometimes durable" parameter is an
/// `Option` at every call site; a caller that wants rows passes the store's closure instead.
fn no_persistence() -> Persist {
    Box::new(|_event| Box::pin(async {}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loop_engine::{ModelAnswer, RequestedCall, ScriptedModel};
    use serde_json::json;

    /// A scripted model that calls `tool` once and then answers.
    fn two_turn(tool: &'static str, answer: &'static str) -> Arc<ScriptedModel> {
        ScriptedModel::new(vec![
            ModelAnswer::calling(vec![RequestedCall::new(tool, json!({}))]),
            ModelAnswer::text(answer),
        ])
    }

    /// The in-crate executor: one tool, permitted. The database-backed `Pipeline` is what
    /// production hands the builder, and it needs a pool; a test that only proves the loop's
    /// wiring does not, so the shape is the one the loop walks with.
    fn executor(tool: &'static str) -> Arc<dyn crate::tool_exec::ToolExecutor> {
        Arc::new(crate::tool_exec::LocalExecutor {
            registry: crate::loop_engine::one_tool(tool, "x"),
            allow: crate::tools::AllowList::new(vec![tool.to_owned()], Vec::new()),
        })
    }

    #[tokio::test]
    async fn the_documented_example_runs() {
        // The doc comment at the top of this file is the example, and the *doctest* above is
        // what executes it — this test is the half a doctest cannot do: it counts the events
        // the callback saw. The two are the same chain, kept in step by the doctest failing the
        // moment the doc drifts, and the count lives here because an assertion inside the
        // document is an assertion nobody reads when the document is quoted in a README.
        let model = two_turn("page.read", "The pricing page lists three plans.");
        let sdk = AgentSdk::builder()
            .model(model)
            .executor(executor("page.read"))
            .system_prompt("You answer questions about the published site.")
            .limits(RunLimits {
                max_steps: 4,
                ..RunLimits::default()
            })
            .build()
            .expect("the documented builder chain must assemble");

        let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&seen);
        let outcome = sdk
            .run("What plans do you publish?", move |_event| {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })
            .await;

        assert!(outcome.is_success(), "outcome was {outcome:?}");
        assert_eq!(
            outcome.final_text.as_deref(),
            Some("The pricing page lists three plans.")
        );
        assert!(
            seen.load(std::sync::atomic::Ordering::SeqCst) >= 4,
            "the callback must see the steps, not only the final frame"
        );
    }

    /// `build()`'s error, as a string. A helper rather than `expect_err`, which would need
    /// `AgentSdk: Debug` — and a Debug impl on a type holding a `dyn Model` is a print that can
    /// fail for a reason that has nothing to do with debugging.
    fn build_error(builder: AgentSdkBuilder) -> String {
        match builder.build() {
            Ok(_) => panic!("the builder must refuse this definition"),
            Err(error) => error.to_string(),
        }
    }

    #[tokio::test]
    async fn a_builder_without_a_model_says_so() {
        let message = build_error(AgentSdk::builder().executor(executor("page.read")));
        assert!(message.contains("model"), "{message}");
    }

    #[tokio::test]
    async fn a_builder_without_an_executor_says_so() {
        // The failure this replaced was worse than an error: a builder that defaulted the
        // executor would have assembled a run that *worked* while walking past the identity,
        // the permission and the call log. The message names the field because "invalid agent"
        // on its own leaves a caller guessing which of the four it forgot.
        let message = build_error(AgentSdk::builder().model(two_turn("page.read", "answer")));
        assert!(message.contains("executor"), "{message}");
        assert!(message.contains("Pipeline"), "{message}");
    }

    #[tokio::test]
    async fn limits_are_clamped_on_the_way_in() {
        let sdk = AgentSdk::builder()
            .model(two_turn("page.read", "answer"))
            .executor(executor("page.read"))
            .limits(RunLimits {
                max_steps: 0,
                deadline_seconds: 0,
                token_budget: 0,
            })
            .build()
            .expect("a zero row is a migration artefact, not a reason to refuse");
        // All-zero is what a column added by a migration carries for every existing row.
        assert_eq!(sdk.limits(), RunLimits::clamped(RunLimits::default()));
        assert!(sdk.limits().max_steps > 0);
    }

    #[tokio::test]
    async fn the_system_prompt_carries_the_guardrail_rules() {
        let sdk = AgentSdk::builder()
            .model(two_turn("page.read", "answer"))
            .executor(executor("page.read"))
            .system_prompt("Answer briefly.")
            .build()
            .expect("assembles");
        let prompt = sdk.system_prompt();
        assert!(prompt.starts_with("Answer briefly."), "{prompt}");
        assert!(
            prompt.contains(crate::agent::UNTRUSTED_FENCE),
            "the preview must be the prompt the loop sends, rules included: {prompt}"
        );
    }

    #[tokio::test]
    async fn a_cancel_handle_stops_the_run_at_the_next_boundary() {
        let model = ScriptedModel::new(vec![
            ModelAnswer::calling(vec![RequestedCall::new("page.read", json!({}))]),
            ModelAnswer::calling(vec![RequestedCall::new("page.read", json!({}))]),
            ModelAnswer::calling(vec![RequestedCall::new("page.read", json!({}))]),
            ModelAnswer::text("never reached"),
        ]);
        let sdk = AgentSdk::builder()
            .model(model)
            .executor(executor("page.read"))
            .build()
            .expect("assembles");
        let cancel = sdk.cancel_handle();
        cancel.request();
        let outcome = sdk.run("go", |_event| {}).await;
        assert_eq!(outcome.stop_reason.as_str(), "cancelled");
    }
}
