//! The real model behind the loop's `Model` trait.
//!
//! The loop is transport-free on purpose (docs/06-AI-HUB.md §5): it asks a `Model` for the next
//! answer and never learns which vendor answered. This module is the other side of that seam —
//! the one implementation that dials a provider. Two things make it more than a one-line
//! wrapper, and both are the kind of bug that costs money rather than crashing:
//!
//! * **A tool-only turn is a valid turn.** A model that answers by calling a tool produces no
//!   text and, on some providers, no finish reason. Treating that as "the provider sent
//!   nothing" ends the run on step one, before the first tool has run.
//! * **The text the loop streams has to be the text the provider sent.** The answer is
//!   reassembled from the stream's deltas and the tool calls from its fragments, so what the
//!   person reads and what the next turn is built from cannot disagree.
//!
//! The call is *streamed* even though the loop wants a whole answer per step: the loop's sink
//! already has a text event, and a model that is asked for a stream is a model whose text can be
//! shown as it arrives rather than after the whole run.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;
use tokio::sync::mpsc;

use crate::client::{
    self, ChatEvent, ChatMessage, ChatRequest, ChatRole, ChatToolCall, ProviderTarget, ToolSpec,
};
use crate::error::AiHubError;
use crate::loop_engine::{Message, Model, ModelAnswer, ModelError, RequestedCall};
use crate::tools::ToolRegistry;

/// How many deltas may queue in front of the loop's sink. The loop already has a bounded sink;
/// this one only exists to carry a single step's deltas to the call that drains them.
const DELTA_CAPACITY: usize = 64;

/// A provider call, behind the loop's `Model` trait.
pub struct ProviderModel {
    target: ProviderTarget,
    /// Wire key of the model — the registry's key, not the provider's own name for it.
    model: String,
    /// What the model is told it may call. Empty means a plain answer, and the adapters then
    /// omit the field rather than sending an empty list some providers answer with a 400.
    tools: Vec<ToolSpec>,
    temperature: Option<f64>,
    max_tokens: Option<u32>,
}

impl ProviderModel {
    /// A model for one resolved `provider/model` pair.
    #[must_use]
    pub fn new(target: ProviderTarget, model: impl Into<String>) -> Self {
        Self {
            target,
            model: model.into(),
            tools: Vec::new(),
            temperature: None,
            max_tokens: None,
        }
    }

    /// The same model, offering a tool registry.
    ///
    /// The registry is read *here* to build the declarations, and again inside the loop to
    /// decide what may run. They are the same object, so a tool the model was told about is a
    /// tool the run can execute — a model told about a tool the loop would refuse produces an
    /// agent that fails on a call it was designed to make.
    ///
    /// **Every caller should prefer [`Self::with_tools`].** This one offers *every* registered
    /// tool, with no identity, no grant and no permission filter — which is exactly what the
    /// execution pipeline exists to prevent. It survives for the crate's own tests, where there
    /// is no database to resolve an identity against; shipping code that calls it would hand a
    /// model tools the pipeline would then refuse, and the run would die on a call it was
    /// designed to make.
    #[must_use]
    pub fn with_registry(mut self, registry: &ToolRegistry) -> Self {
        self.with_tools(registry.catalogue())
    }

    /// The same model, offering exactly the tools the caller decided to show.
    ///
    /// The declarations come from one filter — the execution pipeline's own `model_facing()` —
    /// so a tool that is invisible and a tool that is refused are the same fact computed once.
    #[must_use]
    pub fn with_tools(mut self, visible: Vec<crate::tools::ToolSummary>) -> Self {
        self.tools = visible
            .into_iter()
            // The tool's own schema, not a placeholder: a declaration with empty properties
            // tells the model to call the tool with nothing, and the tool then runs on a
            // missing value instead of the one the model meant.
            .map(|entry| ToolSpec::new(entry.key, entry.description, entry.schema))
            .collect();
        self
    }

    /// Sampling temperature, when the agent sets one.
    #[must_use]
    pub fn temperature(mut self, temperature: Option<f64>) -> Self {
        self.temperature = temperature;
        self
    }

    /// Answer budget in tokens, when the agent sets one.
    #[must_use]
    pub fn max_tokens(mut self, max_tokens: Option<u32>) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// The conversation the loop holds, in the wire's shape.
    ///
    /// This is where the loop's [`Message`] becomes the protocol's request. A `tool` turn quotes
    /// the call it answers and an `assistant` turn carries the calls it made: every protocol
    /// pairs a result to its call by id, so an assistant turn sent without them is a rejected
    /// request rather than a model that lost the thread.
    fn request(&self, messages: &[Message]) -> ChatRequest {
        let wire = messages
            .iter()
            .map(|message| {
                let role = match message.role.as_str() {
                    "assistant" => ChatRole::Assistant,
                    "user" => ChatRole::User,
                    "tool" => ChatRole::Tool,
                    _ => ChatRole::System,
                };
                match role {
                    ChatRole::Tool => ChatMessage::tool_result(
                        message.tool_call_id.as_deref().unwrap_or_default(),
                        message.name.as_deref().unwrap_or_default(),
                        message.content.clone(),
                    ),
                    _ => ChatMessage {
                        role,
                        content: message.content.clone(),
                        tool_call_id: None,
                        name: None,
                        tool_calls: message
                            .tool_calls
                            .iter()
                            .map(|call| ChatToolCall {
                                id: call.id.clone().unwrap_or_default(),
                                name: call.tool.clone(),
                                arguments: call.arguments.clone(),
                            })
                            .collect(),
                    },
                }
            })
            .collect();

        ChatRequest {
            model: self.model.clone(),
            messages: wire,
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            tools: self.tools.clone(),
        }
    }
}

impl Model for ProviderModel {
    fn complete<'a>(
        &'a self,
        step_no: u32,
        messages: &'a [Message],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ModelAnswer, ModelError>> + Send + 'a>>
    {
        Box::pin(async move {
            let request = self.request(messages);
            client::validate_request(&request).map_err(|error| {
                ModelError::new("invalid_request", error.to_string())
            })?;

            // Deltas are collected rather than forwarded: the loop publishes one `text` event
            // per *step*, and a provider that streams a word at a time would otherwise produce
            // thousands of events the store writes a row for.
            let (tx, mut rx) = mpsc::channel::<ChatEvent>(DELTA_CAPACITY);
            let target = self.target.clone();
            let call = tokio::spawn(async move { client::stream_chat(&target, &request, &tx).await });
            let mut deltas = String::new();
            while let Some(event) = rx.recv().await {
                if let ChatEvent::Delta(text) = event {
                    deltas.push_str(&text);
                }
            }

            let outcome = call
                .await
                .map_err(|error| ModelError::new("provider_panic", error.to_string()))?
                .map_err(bridge)?;
            let _ = step_no;

            // Read the fields before the calls are moved out of the outcome.
            let prompt_tokens = outcome
                .usage
                .as_ref()
                .and_then(|usage| usage.prompt_tokens)
                .unwrap_or(0);
            let completion_tokens = outcome
                .usage
                .as_ref()
                .and_then(|usage| usage.completion_tokens)
                .unwrap_or(0);
            let text = if outcome.content.is_empty() {
                deltas
            } else {
                outcome.content
            };
            let calls = outcome.tool_calls;
            // "Final" means nothing was asked of a tool. A provider's own reason string
            // disagrees between the three protocols for the same turn, so the check is on the
            // calls rather than on the string.
            let final_answer = calls.is_empty();

            Ok(ModelAnswer {
                text,
                calls: calls.into_iter().map(requested).collect(),
                prompt_tokens,
                completion_tokens,
                final_answer,
            })
        })
    }
}

/// One wire call as the loop's own shape.
fn requested(call: ChatToolCall) -> RequestedCall {
    RequestedCall {
        tool: call.name,
        arguments: call.arguments,
        id: Some(call.id),
    }
}

/// A provider error in the loop's vocabulary.
///
/// The code is what a consumer branches on and what the run's `error` frame carries, so it is
/// taken from the error kind rather than invented per call site — a run trace that says
/// `upstream` is actionable, one that says `error` is not.
fn bridge(error: AiHubError) -> ModelError {
    let (code, message) = match &error {
        AiHubError::Upstream { status, message } => {
            (code_for_status(*status), message.clone())
        }
        AiHubError::Transport(message) | AiHubError::Stream(message) => ("provider_unreachable", message.clone()),
        AiHubError::Malformed(message) | AiHubError::InvalidChatRequest(message) => {
            ("provider_malformed", message.clone())
        }
        other => ("provider_error", other.to_string()),
    };
    ModelError::new(code, message)
}

/// A stable code for an HTTP status a provider answered with.
fn code_for_status(status: u16) -> &'static str {
    match status {
        401 | 403 => "provider_unauthorized",
        404 => "provider_not_found",
        408 | 504 => "provider_timeout",
        429 => "provider_rate_limited",
        400..=499 => "provider_refused",
        500..=599 => "provider_failed",
        _ => "provider_failed",
    }
}

/// The model a runner spawns: an `Arc` the loop can hold and clone.
pub type SharedModel = Arc<dyn Model>;


// -------------------------------------------------------------------------------------------
// Tests: the real model, against a mock provider, driving the real loop
// -------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::RunLimits;
    use crate::loop_engine::run as run_agent;
    use crate::tools::{AllowList, FnTool, ToolOutcome, ToolRegistry};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use uuid::Uuid;

    /// A mock provider and the request bodies it recorded.
    ///
    /// The bodies are read from a shared handle rather than by joining the mock's task. The
    /// task stays parked in `read` on a socket the client keeps alive for a moment after the
    /// run returns, and awaiting it costs the provider's whole timeout — which is how a suite
    /// that runs in 10 ms came to take 90 s. Bodies are recorded on arrival, so reading them
    /// after the last request gives the same list either way.
    struct MockProvider {
        target: ProviderTarget,
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        served: std::sync::Arc<AtomicUsize>,
    }

    impl MockProvider {
        /// The bodies the provider was sent, in the order they arrived.
        fn bodies(&self) -> Vec<String> {
            self.seen.lock().expect("the mock's own lock").clone()
        }

        /// Wait until the provider has been sent at least `count` requests.
        async fn served_at_least(&self, count: usize) {
            for _ in 0..5_000 {
                if self.served.load(Ordering::SeqCst) >= count {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        }
    }

    /// A provider that answers SSE frames and records the bodies it was sent.
    ///
    /// Four things this has to get right, and each one costs a *hung* test rather than a red
    /// one, which is why they are written down rather than rediscovered every time:
    ///
    /// * **One connection.** The HTTP client pools, so a run's second and third provider calls
    ///   arrive on the *same* socket; a mock that accepts again blocks on a connection that
    ///   never comes.
    /// * **The whole request.** One `read` is not a request: TCP splits wherever it likes, and
    ///   a body straddling a segment boundary would be read as truncated.
    /// * **A length.** A 200 with neither `Content-Length` nor chunked framing leaves the
    ///   client reading until EOF, and a keep-alive socket never sends one.
    /// * **No join.** See [`MockProvider`].
    ///
    /// The recorded bodies are the point of the mock: a loop test that only checks what came
    /// back cannot tell a correctly paired tool result from one a provider would have rejected.
    async fn mock_provider(answers: Vec<Vec<&'static str>>) -> MockProvider {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the mock must bind a port");
        let address = listener.local_addr().expect("the mock has an address");
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let served = Arc::new(AtomicUsize::new(0));
        let recorder = Arc::clone(&seen);
        let counter = Arc::clone(&served);

        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            for frames in &answers {
                let mut raw: Vec<u8> = Vec::new();
                let mut header_end = None;
                while header_end.is_none() {
                    let mut chunk = vec![0_u8; 64 * 1024];
                    let Ok(read) = socket.read(&mut chunk).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    raw.extend_from_slice(&chunk[..read]);
                    header_end = raw
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .map(|at| at + 4);
                }
                let Some(header_end) = header_end else {
                    return;
                };
                let head = String::from_utf8_lossy(&raw[..header_end]).to_string();
                let length: usize = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.trim()
                            .eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().ok())?
                    })
                    .unwrap_or(0);
                let mut body = raw[header_end..].to_vec();
                while body.len() < length {
                    let mut chunk = vec![0_u8; 64 * 1024];
                    let Ok(read) = socket.read(&mut chunk).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    body.extend_from_slice(&chunk[..read]);
                }
                recorder
                    .lock()
                    .expect("the mock's own lock")
                    .push(String::from_utf8_lossy(&body).to_string());
                counter.fetch_add(1, Ordering::SeqCst);

                let mut payload = String::new();
                for frame in frames.iter().copied() {
                    payload.push_str(&format!("data: {frame}\n\n"));
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{payload}",
                    payload.len()
                );
                if socket.write_all(response.as_bytes()).await.is_err() {
                    return;
                }
                let _ = socket.flush().await;
            }
        });

        MockProvider {
            target: ProviderTarget {
                id: Uuid::nil(),
                name: "Mock".to_owned(),
                protocol: "openai_compatible".to_owned(),
                base_url: format!("http://{address}/v1"),
                api_key: None,
                timeout_ms: 10_000,
            },
            seen,
            served,
        }
    }

    /// A provider that refuses every call, with one status and body.
    async fn refusing_provider(status: &'static str) -> ProviderTarget {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buffer = vec![0_u8; 64 * 1024];
                let _ = socket.read(&mut buffer).await;
                let body = "{\"error\":{\"message\":\"quota exhausted\"}}";
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
                break;
            }
        });

        ProviderTarget {
            id: Uuid::nil(),
            name: "Mock".to_owned(),
            protocol: "openai_compatible".to_owned(),
            base_url: format!("http://{address}/v1"),
            api_key: None,
            timeout_ms: 10_000,
        }
    }

    fn tool_registry() -> ToolRegistry {
        ToolRegistry::new(vec![std::sync::Arc::new(
            FnTool::new(
                "page.search",
                "Search the pages.",
                "page.read",
                |arguments| {
                    let q = arguments
                        .get("q")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    ToolOutcome::ok(format!("3 pages match {q}"))
                },
            )
            .with_schema(serde_json::json!({
                "type": "object",
                "properties": { "q": { "type": "string", "description": "What to search for." } },
                "required": ["q"],
            })),
        )])
    }

    fn runtime(model: std::sync::Arc<ProviderModel>) -> crate::loop_engine::Runtime {
        crate::loop_engine::Runtime::for_tests(
            model,
            tool_registry(),
            AllowList::new(vec!["page.search".to_owned()], Vec::new()),
            "You search pages.",
        )
    }

    /// Collect the loop's events while the run is still in flight.
    ///
    /// The channel is unbounded on purpose. A bounded one is what a real sink has, and a test
    /// that fills it blocks inside `publish` — which looks like a hung run rather than a
    /// full queue, and costs the time to diagnose. The events are still all collected.
    async fn collect() -> (mpsc::Sender<crate::agent::AgentEvent>, tokio::task::JoinHandle<Vec<crate::agent::AgentEvent>>) {
        let (tx, mut rx) = mpsc::channel(1024);
        let handle = tokio::spawn(async move {
            let mut events = Vec::new();
            while let Some(event) = rx.recv().await {
                events.push(event);
            }
            events
        });
        (tx, handle)
    }

    #[tokio::test]
    async fn a_plain_answer_comes_back_as_one_step_with_its_tokens() {
        let mock = mock_provider(vec![vec![
            r#"{"choices":[{"delta":{"content":"It is sunny."}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":31,"completion_tokens":6}}"#,
        ]])
        .await;

        let model = std::sync::Arc::new(ProviderModel::new(mock.target.clone(), "mock-model"));
        let runtime = runtime(model);
        let (sink, events) = collect().await;
        let noop: crate::loop_engine::Persist = Box::new(|_| Box::pin(async {}));

        let outcome = run_agent(
            &runtime,
            "what is the weather?",
            RunLimits::default(),
            &sink,
            &noop,
        )
        .await;
        drop(sink);
        let events = events.await.expect("events");

        assert_eq!(outcome.status, "completed");
        assert_eq!(outcome.final_text.as_deref(), Some("It is sunny."));
        assert_eq!(outcome.steps, 1);
        assert!(events.iter().any(|event| matches!(
            event,
            crate::agent::AgentEvent::Usage { prompt_tokens: 31, completion_tokens: 6, .. }
        )));
    }

    #[tokio::test]
    async fn a_tool_call_runs_the_tool_and_the_result_goes_back_paired() {
        let mock = mock_provider(vec![
            vec![r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"page.search","arguments":"{\"q\":\"pricing\"}"}}]},"finish_reason":"tool_calls"}]}"#],
            vec![
                r#"{"choices":[{"delta":{"content":"The pricing page is /pricing."}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ],
        ])
        .await;

        let registry = tool_registry();
        let model =
            std::sync::Arc::new(ProviderModel::new(mock.target.clone(), "mock-model").with_registry(&registry));
        let runtime = runtime(model);
        let (sink, events) = collect().await;
        let noop: crate::loop_engine::Persist = Box::new(|_| Box::pin(async {}));

        let outcome =
            run_agent(&runtime, "find the pricing page", RunLimits::default(), &sink, &noop).await;
        drop(sink);
        let events = events.await.expect("events");
        mock.served_at_least(2).await;
        let bodies = mock.bodies();

        // The tool ran with the arguments the provider actually sent, and its result reached
        // the conversation. An empty argument object would run the tool with no query and
        // produce "3 pages match " — the failure this assertion is written to catch.
        let result = events.iter().find_map(|event| match event {
            crate::agent::AgentEvent::ToolResult { tool, summary, .. } if tool == "page.search" => {
                Some(summary.clone())
            }
            _ => None,
        });
        let errors: Vec<String> = events
            .iter()
            .filter_map(|event| match event {
                crate::agent::AgentEvent::Error { code, message, .. } => {
                    Some(format!("{code}: {message}"))
                }
                _ => None,
            })
            .collect();
        assert!(
            result
                .as_deref()
                .is_some_and(|summary| summary.contains("3 pages match pricing")),
            "the tool's result never reached the transcript: {result:?} errors={errors:?}"
        );
        assert_eq!(outcome.status, "completed", "the run answered: {outcome:?} errors={errors:?}");
        assert_eq!(
            outcome.final_text.as_deref(),
            Some("The pricing page is /pricing.")
        );
        assert_eq!(bodies.len(), 2, "the run made two provider calls");
    }

    #[tokio::test]
    async fn the_second_request_quotes_the_call_the_first_one_made() {
        // One call, then the final answer. The tool runs between them, so the second body has
        // to carry the assistant turn *and* the paired result — and that body is the whole
        // contract between the loop and any provider that insists on pairing.
        let mock = mock_provider(vec![
            vec![r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_42","function":{"name":"page.search","arguments":"{\"q\":\"x\"}"}}]},"finish_reason":"tool_calls"}]}"#],
            vec![
                r#"{"choices":[{"delta":{"content":"Found it."}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ],
        ])
        .await;

        let registry = tool_registry();
        let model =
            std::sync::Arc::new(ProviderModel::new(mock.target.clone(), "mock-model").with_registry(&registry));
        let runtime = runtime(model);
        let (sink, events) = collect().await;
        let noop: crate::loop_engine::Persist = Box::new(|_| Box::pin(async {}));

        let outcome = run_agent(&runtime, "find x", RunLimits::default(), &sink, &noop).await;
        drop(sink);
        let events = events.await.expect("events");
        mock.served_at_least(2).await;
        let bodies = mock.bodies();

        let errors: Vec<String> = events
            .iter()
            .filter_map(|event| match event {
                crate::agent::AgentEvent::Error { code, message, .. } => {
                    Some(format!("{code}: {message}"))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            outcome.status,
            "completed",
            "the run answered: {outcome:?} errors={errors:?} bodies={bodies:?}"
        );
        assert_eq!(bodies.len(), 2, "the run made two provider calls");

        let second: Value = serde_json::from_str(&bodies[1]).expect("the second body is JSON");
        let messages = second["messages"].as_array().expect("messages");
        let roles: Vec<&str> = messages
            .iter()
            .filter_map(|message| message["role"].as_str())
            .collect();
        assert_eq!(
            roles,
            vec!["system", "user", "assistant", "tool"],
            "the second request must be system, goal, the call and its result: {roles:?}"
        );
        // The assistant turn quotes the call the provider made, and the result quotes it back.
        assert_eq!(messages[2]["tool_calls"][0]["id"], "call_42");
        assert_eq!(messages[2]["tool_calls"][0]["function"]["name"], "page.search");
        assert_eq!(messages[3]["tool_call_id"], "call_42");
        assert!(messages[3]["content"]
            .as_str()
            .is_some_and(|content| content.contains("3 pages match x")));
        // And the tool was declared on both calls, so the model is offered the same tool the
        // run is allowed to execute.
        assert_eq!(second["tools"][0]["function"]["name"], "page.search");
        let first: Value = serde_json::from_str(&bodies[0]).expect("the first body is JSON");
        assert_eq!(first["tools"][0]["function"]["name"], "page.search");
        assert!(events
            .iter()
            .any(|event| matches!(event, crate::agent::AgentEvent::Text { .. })));
    }

    #[tokio::test]
    async fn a_refusing_provider_becomes_a_code_the_run_trace_can_branch_on() {
        let target = refusing_provider("429 Too Many Requests").await;
        let model = std::sync::Arc::new(ProviderModel::new(target, "mock-model"));
        let runtime = runtime(model);
        let (sink, events) = collect().await;
        let noop: crate::loop_engine::Persist = Box::new(|_| Box::pin(async {}));

        let outcome = run_agent(&runtime, "anything", RunLimits::default(), &sink, &noop).await;
        drop(sink);
        let events = events.await.expect("events");

        assert_eq!(outcome.status, "failed");
        let error = events
            .iter()
            .find_map(|event| match event {
                crate::agent::AgentEvent::Error { code, .. } => Some(code.clone()),
                _ => None,
            })
            .expect("the run published an error");
        // 429 has to be distinguishable from a 400 in a trace: one is "wait", the other is
        // "the request is wrong", and an operator reads them differently.
        assert_eq!(error, "provider_rate_limited");
    }

    #[tokio::test]
    async fn an_agent_with_no_tools_offers_none_rather_than_an_empty_list() {
        let mock = mock_provider(vec![vec![
            r#"{"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}"#,
        ]])
        .await;
        let model = std::sync::Arc::new(ProviderModel::new(mock.target.clone(), "mock-model"));
        let runtime = crate::loop_engine::Runtime::for_tests(
            model,
            ToolRegistry::empty(),
            AllowList::new(Vec::new(), Vec::new()),
            "",
        );
        let (sink, _events) = collect().await;
        let noop: crate::loop_engine::Persist = Box::new(|_| Box::pin(async {}));

        let _ = run_agent(&runtime, "hi", RunLimits::default(), &sink, &noop).await;
        drop(sink);
        mock.served_at_least(1).await;
        let bodies = mock.bodies();
        let body: Value = serde_json::from_str(&bodies[0]).expect("JSON");
        assert!(
            body.get("tools").is_none(),
            "an agent with no tools must not send a tools field at all"
        );
    }

    /// Does the mock survive two *sequential* provider calls on one pooled connection?
    ///
    /// This is the seam the loop depends on and the reason a whole class of mock hangs is
    /// diagnosable: if two calls in a row cannot complete, the loop test is measuring the
    /// mock, not the code.
    #[tokio::test]
    async fn the_mock_itself_serves_two_sequential_streamed_calls() {
        let mock = mock_provider(vec![
            vec![r#"{"choices":[{"delta":{"content":"first"},"finish_reason":"stop"}]}"#],
            vec![r#"{"choices":[{"delta":{"content":"second"},"finish_reason":"stop"}]}"#],
        ])
        .await;

        for expected in ["first", "second"] {
            let request = ChatRequest::new("mock-model", vec![ChatMessage::user("hi")]);
            let (tx, mut rx) = mpsc::channel(8);
            let target = mock.target.clone();
            let call = tokio::spawn(async move { client::stream_chat(&target, &request, &tx).await });
            while let Some(_event) = rx.recv().await {}
            let outcome = call.await.expect("the call task").expect("a streamed answer");
            assert_eq!(outcome.content, expected);
        }

        let bodies = mock.bodies();
        assert_eq!(bodies.len(), 2, "both request bodies were recorded");
    }

    /// Does the loop itself complete a tool-then-answer run, with no provider involved?
    ///
    /// This isolates the tool path from the wire: if it hangs with a scripted model, the
    /// block is in the loop's handling of a tool result, and no amount of mock tuning finds it.
    #[tokio::test]
    async fn the_loop_alone_completes_a_tool_then_answer_run() {
        use crate::loop_engine::{ModelAnswer, RequestedCall, Runtime};
        let model = crate::loop_engine::ScriptedModel::new(vec![
            ModelAnswer::calling(vec![RequestedCall::new("page.search", serde_json::json!({ "q": "x" }))]),
            ModelAnswer::text("done"),
        ]);
        let runtime = Runtime::for_tests(
            model,
            tool_registry(),
            AllowList::new(vec!["page.search".to_owned()], Vec::new()),
            "prompt",
        );
        let (sink, events) = collect().await;
        let noop: crate::loop_engine::Persist = Box::new(|_| Box::pin(async {}));

        let outcome = run_agent(&runtime, "go", RunLimits::default(), &sink, &noop).await;
        drop(sink);
        let events = events.await.expect("events");

        assert_eq!(outcome.status, "completed", "{outcome:?}");
        assert_eq!(outcome.steps, 2);
        assert!(!events.is_empty());
    }
}
