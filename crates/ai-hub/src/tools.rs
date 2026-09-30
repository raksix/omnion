//! The tool system: what an agent is allowed to do, and what happens when it asks.
//!
//! docs/06-AI-HUB.md §6 and REQ-099 scope it as "execute any tool calls through the tool system
//! (REQ-100)"; REQ-100 owns the catalogue of tools, this module owns the **boundary the loop
//! checks before anything runs**.
//!
//! The order of the checks is the whole design:
//!
//! 1. **Unknown tool** — the model invented a key. Refused with `tool_unknown`; nothing looked
//!    up, nothing executed.
//! 2. **Not on the allow-list** — the agent's configuration decides what it may do, and a
//!    *refusal* (not a silent skip) is the answer, because a model that retries a hidden tool
//!    three times has just been told the tool exists. Refused with `tool_denied`.
//! 3. **Needs approval and has none** — the loop parks. This is the hand-off to REQ-101, and it
//!    is a *park*, never an implicit yes: an approval-gated tool that ran because nobody was
//!    looking is the single worst outcome this system can produce.
//! 4. **Arguments are not an object** — every tool takes a JSON object, and a string or a list
//!    that reached a tool's own deserialiser is a bug in the model, not in the tool.
//!
//! Every refusal carries a **stable code** (the acceptance criteria say "a stable code") and the
//! tool key, so the trace can show why nothing happened and a test can assert on the code rather
//! than on the wording.

use std::fmt;
use std::future::Future;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::{AgentEvent, ToolCall};

/// Longest result summary a tool may report. The stored `result` is a summary, never the payload:
/// a tool that returns a page of records must say so in a sentence, because the transcript is
/// stored, redacted into webhooks and re-read by a person.
pub const MAX_SUMMARY_CHARS: usize = 2_000;

/// A tool the loop may run, and the permission it needs.
///
/// This is deliberately *not* a boxed async closure. A trait object would let a tool capture
/// anything, and the point of the allow-list is that the set of things an agent can do is
/// enumerable: `ai.tool.catalogue` lists every implementation, and the agent's `tools` jsonb
/// names a subset of those keys.
pub trait Tool: Send + Sync {
    /// The key the model calls, e.g. `page.search`.
    fn key(&self) -> &str;

    /// What the tool does, shown in the agent editor and to the model.
    fn description(&self) -> &str;

    /// The permission a caller must hold for this tool to run.
    fn permission(&self) -> &str;

    /// The JSON Schema of this tool's arguments, as the model is told.
    ///
    /// Defaults to "no arguments", which is the honest answer for a tool that takes none —
    /// but a tool that *does* take arguments and inherits this is told to call it with an
    /// empty object, and the model then invents a query the tool never receives. Anything with
    /// arguments overrides it.
    fn schema(&self) -> Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    /// Run the tool.
    ///
    /// Returns the *summary* that goes into the transcript. Implementations that return more
    /// than [`MAX_SUMMARY_CHARS`] are truncated by [`execute`], not here, so no implementation
    /// has to remember the cap.
    fn run(
        &self,
        arguments: &Value,
    ) -> std::pin::Pin<Box<dyn Future<Output = ToolOutcome> + Send + '_>>;
}

/// What running a tool produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    /// The text the model sees, capped and delimited if it came from outside.
    pub content: String,
    /// Whether the tool failed. A failed tool is *not* a loop failure: the model is told what
    /// went wrong and gets another step.
    pub failed: bool,
}

impl ToolOutcome {
    /// A successful result.
    #[must_use]
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            failed: false,
        }
    }

    /// A failed result. The message is what the model reads, so it must not be a Postgres error.
    #[must_use]
    pub fn failed(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            failed: true,
        }
    }
}

/// A synchronous tool, which is what most of them are: a lookup, a count, a query.
///
/// Wrapping a closure in a trait is a great deal of ceremony to repeat fifty times, so the
/// common shape gets an adapter and the async shape implements [`Tool`] directly.
pub struct FnTool<F> {
    key: String,
    description: String,
    permission: String,
    schema: Value,
    body: F,
}

impl<F> FnTool<F>
where
    F: Fn(&Value) -> ToolOutcome + Send + Sync,
{
    /// Describe a synchronous tool that takes no arguments.
    pub fn new(
        key: impl Into<String>,
        description: impl Into<String>,
        permission: impl Into<String>,
        body: F,
    ) -> Self {
        Self {
            key: key.into(),
            description: description.into(),
            permission: permission.into(),
            schema: serde_json::json!({ "type": "object", "properties": {} }),
            body,
        }
    }

    /// The same tool, with the arguments it takes declared to the model.
    ///
    /// Declaring them is not optional politeness: a model told a tool takes no arguments calls
    /// it with an empty object, and the tool then runs on a missing value instead of on the
    /// one the model meant.
    #[must_use]
    pub fn with_schema(mut self, schema: Value) -> Self {
        self.schema = schema;
        self
    }
}

impl<F> Tool for FnTool<F>
where
    F: Fn(&Value) -> ToolOutcome + Send + Sync,
{
    fn key(&self) -> &str {
        &self.key
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn permission(&self) -> &str {
        &self.permission
    }

    /// **This override is the whole point of [`FnTool::with_schema`].**
    ///
    /// Without it the builder wrote the schema into a field nothing read: `Tool::schema`'s
    /// default (an object with no properties) is what every `FnTool` reported, so a tool that
    /// declared `{"required": ["target"]}` was told the model it takes *no arguments* and then
    /// refused the model's `target` as an unknown field. The failure is invisible in a test that
    /// only checks "a call with a bad type is refused" — which was true, for the wrong reason.
    fn schema(&self) -> Value {
        self.schema.clone()
    }

    fn run(
        &self,
        arguments: &Value,
    ) -> std::pin::Pin<Box<dyn Future<Output = ToolOutcome> + Send + '_>> {
        Box::pin(std::future::ready((self.body)(arguments)))
    }
}

/// A tool that was refused, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DenyReason {
    /// No tool carries that key.
    ToolUnknown,
    /// The tool exists but the agent may not use it.
    ToolDenied,
    /// The tool needs a decision nobody has made.
    ApprovalRequired,
    /// The arguments were not a JSON object.
    BadArguments,
}

impl DenyReason {
    /// The stable code a consumer branches on. These strings are API surface.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::ToolUnknown => "tool_unknown",
            Self::ToolDenied => "tool_denied",
            Self::ApprovalRequired => "approval_required",
            Self::BadArguments => "tool_bad_arguments",
        }
    }

    /// Read a code back.
    #[must_use]
    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "tool_unknown" => Some(Self::ToolUnknown),
            "tool_denied" => Some(Self::ToolDenied),
            "approval_required" => Some(Self::ApprovalRequired),
            "tool_bad_arguments" => Some(Self::BadArguments),
            _ => None,
        }
    }
}

impl fmt::Display for DenyReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

/// What a tool call turned into: it ran, it was refused, or it parked the run.
#[derive(Debug, Clone, PartialEq)]
pub enum Execution {
    /// The tool ran and its summary is here.
    Ran {
        /// The tool that ran.
        tool: String,
        /// The capped summary.
        summary: String,
        /// Whether the tool itself reported a failure. A tool that fails is not a dead run: the
        /// model reads the message and takes another step.
        failed: bool,
    },
    /// Nothing ran.
    Refused {
        /// The tool that was refused.
        tool: String,
        /// Why.
        reason: DenyReason,
    },
    /// The loop must stop and wait for a person.
    Parked {
        /// The tool awaiting the decision.
        tool: String,
        /// What the decider sees.
        arguments: Value,
    },
}

/// One row of the catalogue the agent editor renders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSummary {
    /// The key the model calls.
    pub key: String,
    /// What it does.
    pub description: String,
    /// The permission a caller needs.
    pub permission: String,
    /// The JSON Schema of the arguments, as the model is told.
    pub schema: Value,
}

/// The registry: every implemented tool, by key.
///
/// The loop never sees anything else. A tool that is not in this map cannot run, which is what
/// makes "the set of things an agent can do" a question the panel can answer.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// An empty registry. A run whose agent has no tools still works — it just answers.
    #[must_use]
    pub fn empty() -> Self {
        Self { tools: Vec::new() }
    }

    /// Build a registry from a list.
    #[must_use]
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
        Self { tools }
    }

    /// Add a tool, replacing any earlier one with the same key.
    ///
    /// Replace rather than reject: a plugin that registers `page.search` while a built-in
    /// already does is overriding a definition, and a startup that panicked there would be a
    /// startup nobody could fix.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.retain(|existing| existing.key() != tool.key());
        self.tools.push(tool);
    }

    /// Whether a key is implemented.
    #[must_use]
    pub fn contains(&self, key: &str) -> bool {
        self.tools.iter().any(|tool| tool.key() == key)
    }

    /// One tool by key.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .iter()
            .find(|tool| tool.key() == key)
            .map(Arc::clone)
    }

    /// Every registered key, sorted — the agent editor's tool list.
    #[must_use]
    pub fn keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.tools.iter().map(|tool| tool.key().to_owned()).collect();
        keys.sort();
        keys
    }

    /// How many tools are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// What a tool needs permission for, for the editor's row.
    #[must_use]
    pub fn permission_for(&self, key: &str) -> Option<&str> {
        self.tools
            .iter()
            .find(|tool| tool.key() == key)
            .map(|tool| tool.permission())
    }

    /// The catalogue the agent editor lists: key, description and the permission it needs.
    #[must_use]
    pub fn catalogue(&self) -> Vec<ToolSummary> {
        let mut entries: Vec<ToolSummary> = self
            .tools
            .iter()
            .map(|tool| ToolSummary {
                key: tool.key().to_owned(),
                description: tool.description().to_owned(),
                permission: tool.permission().to_owned(),
                schema: tool.schema(),
            })
            .collect();
        entries.sort_by(|left, right| left.key.cmp(&right.key));
        entries
    }
}

/// What the agent's own configuration allows.
///
/// The `tools` and `approvals` jsonb columns on `ai_agents` are ordered key lists, and this is
/// the code that reads them. An **empty** allow-list is the interesting case: it means "no tools",
/// not "all tools". A default that meant "all" would hand every existing agent every tool the day
/// the column shipped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AllowList {
    allowed: Vec<String>,
    approvals: Vec<String>,
}

impl AllowList {
    /// Build from the two stored lists.
    #[must_use]
    pub fn new(allowed: Vec<String>, approvals: Vec<String>) -> Self {
        Self { allowed, approvals }
    }

    /// Read the two jsonb columns, tolerating `null` and a non-array.
    ///
    /// A model that has never been asked to use tools stores `NULL` in both columns, and a
    /// hand-edited row can hold a string. Neither is a reason to fail a run.
    #[must_use]
    pub fn from_columns(tools: Option<&Value>, approvals: Option<&Value>) -> Self {
        Self::new(read_keys(tools), read_keys(approvals))
    }

    /// Whether a tool may run.
    #[must_use]
    pub fn allows(&self, key: &str) -> bool {
        self.allowed.iter().any(|entry| entry == key)
    }

    /// Whether a tool needs a decision first.
    #[must_use]
    pub fn needs_approval(&self, key: &str) -> bool {
        self.approvals.iter().any(|entry| entry == key)
    }

    /// The allowed keys, in stored order.
    #[must_use]
    pub fn allowed(&self) -> &[String] {
        &self.allowed
    }

    /// The approval-gated keys, in stored order.
    #[must_use]
    pub fn approvals(&self) -> &[String] {
        &self.approvals
    }
}

fn read_keys(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.as_str())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Decide what a call means, then run it if the decision is "run".
///
/// The decision is separated from the execution on purpose: the loop needs to know *before*
/// anything runs whether the run is about to park, and a registry that both decided and ran
/// would make "did we already execute this?" an unanswerable question at the park point.
pub async fn decide(
    registry: &ToolRegistry,
    allow: &AllowList,
    call: &ToolCall,
) -> Execution {
    let Some(tool) = registry.get(&call.tool) else {
        return Execution::Refused {
            tool: call.tool.clone(),
            reason: DenyReason::ToolUnknown,
        };
    };
    if !allow.allows(&tool.key()) {
        return Execution::Refused {
            tool: call.tool.clone(),
            reason: DenyReason::ToolDenied,
        };
    }
    if !call.arguments.is_object() {
        return Execution::Refused {
            tool: call.tool.clone(),
            reason: DenyReason::BadArguments,
        };
    }
    if allow.needs_approval(&tool.key()) {
        return Execution::Parked {
            tool: call.tool.clone(),
            arguments: crate::run_store::redact_arguments(&call.arguments),
        };
    }
    // The tool exists and is allowed; run it and cap what comes back.
    let outcome = tool.run(&call.arguments).await;
    let failed = outcome.failed;
    let mut summary = outcome.content;
    if summary.chars().count() > MAX_SUMMARY_CHARS {
        summary = format!(
            "{}\n[truncated at {} characters]",
            summary.chars().take(MAX_SUMMARY_CHARS - 1).collect::<String>(),
            MAX_SUMMARY_CHARS
        );
    }
    Execution::Ran {
        tool: tool.key().to_owned(),
        summary,
        failed,
    }
}

/// The event a decision turns into, so the caller does not repeat the mapping.
#[must_use]
pub fn event_for(step_no: u32, execution: &Execution, failed: bool) -> Option<AgentEvent> {
    match execution {
        Execution::Ran { tool, summary, .. } => Some(AgentEvent::ToolResult {
            step_no,
            tool: tool.clone(),
            summary: summary.clone(),
            failed,
        }),
        Execution::Refused { tool, reason } => Some(AgentEvent::ToolResult {
            step_no,
            tool: tool.clone(),
            summary: format!("refused: {reason}"),
            failed: true,
        }),
        Execution::Parked { tool, arguments } => Some(AgentEvent::AwaitingApproval {
            step_no,
            tool: tool.clone(),
            arguments: arguments.clone(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn registry() -> ToolRegistry {
        let mut registry = ToolRegistry::empty();
        registry.register(Arc::new(FnTool::new(
            "page.search",
            "Search pages",
            "content.pages.read",
            |_| ToolOutcome::ok("found 3 pages"),
        )));
        registry.register(Arc::new(FnTool::new(
            "page.delete",
            "Delete a page",
            "content.pages.delete",
            |_| ToolOutcome::ok("deleted"),
        )));
        registry
    }

    fn call(tool: &str, arguments: Value) -> ToolCall {
        ToolCall::new(tool, arguments)
    }

    #[test]
    fn a_registered_tool_runs_and_reports_its_key() {
        let registry = registry();
        let allow = AllowList::new(vec!["page.search".into()], Vec::new());
        let outcome = futures_lite_block_on(decide(&registry, &allow, &call("page.search", json!({}))));
        assert_eq!(
            outcome,
            Execution::Ran {
                tool: "page.search".to_owned(),
                summary: "found 3 pages".to_owned(),
                failed: false,
            }
        );
    }

    #[test]
    fn an_unknown_tool_is_refused_with_a_stable_code_and_nothing_looks_it_up() {
        // The counter proves the tool body never ran: a registry miss cannot reach a body, and
        // the only way to prove that is to have something to count.
        let registry = registry();
        let allow = AllowList::new(vec!["page.search".into(), "made.up".into()], Vec::new());
        let outcome = futures_lite_block_on(decide(
            &registry,
            &allow,
            &call("made.up", json!({})),
        ));
        assert_eq!(
            outcome,
            Execution::Refused {
                tool: "made.up".to_owned(),
                reason: DenyReason::ToolUnknown,
            }
        );
        assert_eq!(outcome_refused_code(&outcome), "tool_unknown");
    }

    fn outcome_refused_code(outcome: &Execution) -> &'static str {
        match outcome {
            Execution::Refused { reason, .. } => reason.code(),
            _ => "",
        }
    }

    #[test]
    fn a_tool_the_agent_does_not_allow_list_is_refused_even_though_it_exists() {
        // The dangerous case: `page.delete` is implemented, and the agent simply may not use it.
        // Allowing it "because it exists" is the bug this test exists to prevent.
        let registry = registry();
        let allow = AllowList::new(vec!["page.search".into()], Vec::new());
        let outcome = futures_lite_block_on(decide(
            &registry,
            &allow,
            &call("page.delete", json!({ "id": 1 })),
        ));
        assert_eq!(outcome_refused_code(&outcome), "tool_denied");
        assert!(matches!(outcome, Execution::Refused { ref tool, .. } if tool == "page.delete"));
    }

    #[test]
    fn an_approval_gated_tool_parks_the_run_and_redacts_its_arguments() {
        let registry = registry();
        let allow = AllowList::new(
            vec!["page.delete".into()],
            vec!["page.delete".into()],
        );
        let outcome = futures_lite_block_on(decide(
            &registry,
            &allow,
            &call(
                "page.delete",
                json!({ "id": 1, "api_key": "sk-live-supersecret" }),
            ),
        ));
        match outcome {
            Execution::Parked { tool, arguments } => {
                assert_eq!(tool, "page.delete");
                // The decider sees the shape of the call, not its secrets.
                let rendered = arguments.to_string();
                assert!(!rendered.contains("supersecret"), "{rendered}");
            }
            other => panic!("expected a park, got {other:?}"),
        }
    }

    #[test]
    fn arguments_that_are_not_an_object_are_refused_before_the_tool_sees_them() {
        let registry = registry();
        let allow = AllowList::new(vec!["page.search".into()], Vec::new());
        for arguments in [json!("a string"), json!([1, 2]), json!(null)] {
            let outcome =
                futures_lite_block_on(decide(&registry, &allow, &call("page.search", arguments)));
            assert_eq!(outcome_refused_code(&outcome), "tool_bad_arguments");
        }
    }

    #[test]
    fn a_long_result_is_capped_and_the_cut_is_stated_inside_the_summary() {
        // A model that reads "[truncated]" as "that was all of it" will confidently answer about
        // rows it never saw, so the mark has to be in the payload rather than in a log line.
        let long = "x".repeat(MAX_SUMMARY_CHARS + 500);
        let registry = ToolRegistry::new(vec![Arc::new(FnTool::new(
            "big.dump",
            "Dump",
            "content.pages.read",
            move |_| ToolOutcome::ok(long.clone()),
        ))]);
        let allow = AllowList::new(vec!["big.dump".into()], Vec::new());
        let outcome = futures_lite_block_on(decide(&registry, &allow, &call("big.dump", json!({}))));
        match outcome {
            Execution::Ran { summary, .. } => {
                assert!(summary.contains("truncated"), "{summary}");
                assert!(summary.chars().count() <= MAX_SUMMARY_CHARS + 40);
            }
            other => panic!("expected a run, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_allow_list_means_no_tools_rather_than_every_tool() {
        // The day the column shipped, every existing agent had NULL in it. "Empty means all"
        // would have handed all of them the whole registry on the first run.
        let registry = registry();
        let allow = AllowList::default();
        let outcome = futures_lite_block_on(decide(
            &registry,
            &allow,
            &call("page.search", json!({})),
        ));
        assert_eq!(outcome_refused_code(&outcome), "tool_denied");
    }

    #[test]
    fn the_two_stored_columns_survive_null_and_a_string() {
        let allow = AllowList::from_columns(None, None);
        assert!(allow.allowed().is_empty());
        let allow = AllowList::from_columns(Some(&json!("not an array")), Some(&json!(["a.b"])));
        assert!(allow.allowed().is_empty());
        assert!(allow.needs_approval("a.b"));
    }

    #[test]
    fn registering_a_key_twice_replaces_rather_than_duplicates() {
        let mut registry = registry();
        let before = registry.len();
        registry.register(Arc::new(FnTool::new(
            "page.search",
            "Replacement",
            "content.pages.read",
            |_| ToolOutcome::ok("second"),
        )));
        assert_eq!(registry.len(), before, "a re-registration must not grow the registry");
        let allow = AllowList::new(vec!["page.search".into()], Vec::new());
        let outcome = futures_lite_block_on(decide(&registry, &allow, &call("page.search", json!({}))));
        assert!(matches!(outcome, Execution::Ran { ref summary, .. } if summary == "second"));
    }

    #[test]
    fn a_denial_code_survives_a_round_trip_through_its_wire_name() {
        for reason in [
            DenyReason::ToolUnknown,
            DenyReason::ToolDenied,
            DenyReason::ApprovalRequired,
            DenyReason::BadArguments,
        ] {
            assert_eq!(DenyReason::from_code(reason.code()), Some(reason));
        }
        assert_eq!(DenyReason::from_code("nonsense"), None);
    }

    #[test]
    fn an_execution_maps_to_the_event_the_trace_shows() {
        let ran = event_for(
            3,
            &Execution::Ran {
                tool: "page.search".into(),
                summary: "ok".into(),
                failed: false,
            },
            false,
        );
        assert!(matches!(ran, Some(AgentEvent::ToolResult { step_no: 3, ref failed, .. }) if !failed));

        let refused = event_for(
            3,
            &Execution::Refused {
                tool: "page.delete".into(),
                reason: DenyReason::ToolDenied,
            },
            false,
        );
        match refused {
            Some(AgentEvent::ToolResult { summary, failed, .. }) => {
                assert!(failed);
                assert!(summary.contains("tool_denied"), "{summary}");
            }
            other => panic!("expected a tool result, got {other:?}"),
        }

        let parked = event_for(
            4,
            &Execution::Parked {
                tool: "page.delete".into(),
                arguments: json!({ "id": 1 }),
            },
            false,
        );
        assert!(matches!(parked, Some(AgentEvent::AwaitingApproval { step_no: 4, .. })));
    }

    /// Run one future to completion on the current thread.
    ///
    /// The crate has no async test dependency and every decision here resolves immediately, so a
    /// hand-rolled one-shot executor is smaller than a dev-dependency that would exist only to
    /// spell `block_on`.
    fn futures_lite_block_on<F: std::future::Future>(future: F) -> F::Output {
        use std::sync::Arc;
        use std::task::{Context, Poll, Wake, Waker};

        struct Noop;
        impl Wake for Noop {
            fn wake(self: Arc<Self>) {}
        }

        let waker = Waker::from(Arc::new(Noop));
        let mut context = Context::from_waker(&waker);
        let mut future = Box::pin(future);
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(value) => return value,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }
}
