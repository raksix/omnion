//! The wire client: one OpenAI-compatible protocol behind the AI Hub's own types.
//!
//! Everything the platform needs in v0 goes through this module, and nothing inside Omnion knows
//! which vendor answers: `resolve` picks a provider and a model, the client speaks to it, and
//! callers see ([`ChatRequest`] → [`ChatOutcome`]) — or, for streaming,
//! ([`ChatEvent::Delta`] … → [`ChatOutcome`]) (docs/06-AI-HUB.md §2: "the rest of Omnion does
//! not know which provider is in use").
//!
//! Two calls are implemented: `chat` (one JSON answer) and `stream_chat` (server-sent events,
//! pushed onto a channel the caller owns). `list_remote_models` reads the provider's own model
//! list so the panel can offer discovery instead of hand-typed keys.
//!
//! The protocol details that matter are handled here and nowhere else: the `Authorization`
//! header, the message shape, the `[DONE]` sentinel, providers that answer a stream request
//! without the usage block, and providers that report a failure *inside* an otherwise `200`
//! stream.

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::error::{AiHubError, Result};
use crate::model::Provider;
use crate::protocol::{StreamPiece, adapter_for};

/// Time allowed to open the connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest error body kept from a refusing provider.
const MAX_ERROR_BODY: usize = 400;
/// Longest answer accepted from a provider, in bytes — a guard against a runaway stream.
const MAX_ANSWER_BYTES: usize = 4 * 1024 * 1024;

/// One provider addressed for a call.
#[derive(Debug, Clone)]
pub struct ProviderTarget {
    /// Provider id (for logging and audit).
    pub id: Uuid,
    /// Display name.
    pub name: String,
    /// Wire protocol.
    pub protocol: String,
    /// Base URL, no trailing slash.
    pub base_url: String,
    /// Key to authenticate with.
    pub api_key: Option<String>,
    /// How long one call may take, in milliseconds.
    pub timeout_ms: u64,
}

impl ProviderTarget {
    /// Address a stored provider.
    #[must_use]
    pub fn from_provider(provider: &Provider) -> Self {
        Self {
            id: provider.id,
            name: provider.name.clone(),
            protocol: provider.protocol.clone(),
            base_url: provider.base_url.trim_end_matches('/').to_owned(),
            api_key: provider.api_key.clone().filter(|key| !key.is_empty()),
            timeout_ms: provider.timeout_ms.max(1) as u64,
        }
    }

    /// Full URL of one endpoint below the base URL.
    ///
    /// A trailing slash on the stored URL never becomes a double slash here, whatever built the
    /// target.
    #[must_use]
    pub fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), path)
    }
}

/// Role of one chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    /// Instructions for the model.
    System,
    /// What the person asked.
    User,
    /// What the model answered earlier in the same conversation.
    Assistant,
    /// The result of a tool the model asked for.
    Tool,
}

impl ChatRole {
    /// Wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            // No protocol takes a `tool` role on a request message: OpenAI spells the result as
            // `role: "tool"`, the other two carry it as a content block. The name is here for
            // logs and for the round trip through `parse`, not for a body we hand a provider.
            Self::Tool => "tool",
        }
    }

    /// Read a wire name.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "system" => Ok(Self::System),
            "user" => Ok(Self::User),
            "assistant" => Ok(Self::Assistant),
            "tool" => Ok(Self::Tool),
            other => Err(AiHubError::InvalidChatRequest(format!(
                "unknown message role \"{other}\" (system, user, assistant, tool)"
            ))),
        }
    }
}

/// One tool the model may call, in the platform's own shape.
///
/// Every adapter translates this into its own spelling; a caller never writes a vendor's tool
/// JSON. `parameters` is a JSON Schema object — the same schema the tool's arguments are
/// validated against before anything runs, so what the model is told and what the tool checks
/// are the same document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Key the call will carry.
    pub name: String,
    /// What the tool does, in one line.
    #[serde(default)]
    pub description: String,
    /// JSON Schema of the arguments.
    #[serde(default)]
    pub parameters: Value,
}

impl ToolSpec {
    /// A tool with a description and an argument schema.
    #[must_use]
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }
}

/// One message of a chat request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Who said it.
    pub role: ChatRole,
    /// What was said.
    pub content: String,
    /// Set on a [`ChatRole::Tool`] message: the call this result answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Set on a [`ChatRole::Tool`] message: which tool ran. Two protocols need it and neither can
    /// be served from the id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Set on a [`ChatRole::Assistant`] message: the tool calls the model made in that turn.
    ///
    /// The turn has to travel back to the provider with its calls attached, because the results
    /// that follow quote the call ids — and a result whose call is missing from the
    /// conversation is a rejected request, not a model that lost the thread.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ChatToolCall>,
}

impl ChatMessage {
    /// A user message.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::User,
            content: content.into(),
            tool_call_id: None,
            name: None,
            tool_calls: Vec::new(),
        }
    }

    /// A system message.
    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::System,
            content: content.into(),
            tool_call_id: None,
            name: None,
            tool_calls: Vec::new(),
        }
    }

    /// An assistant message from an earlier turn.
    #[must_use]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::Assistant,
            content: content.into(),
            tool_call_id: None,
            name: None,
            tool_calls: Vec::new(),
        }
    }

    /// The result of one tool call, handed back to the model.
    #[must_use]
    pub fn tool_result(
        call_id: impl Into<String>,
        tool: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            role: ChatRole::Tool,
            content: content.into(),
            tool_call_id: Some(call_id.into()),
            name: Some(tool.into()),
            tool_calls: Vec::new(),
        }
    }

    /// The id this message answers, for the protocols that need one.
    #[must_use]
    pub fn call_id(&self) -> &str {
        self.tool_call_id.as_deref().unwrap_or_default()
    }

    /// The tool this message reports on, for the protocols that need the name.
    #[must_use]
    pub fn tool_name(&self) -> &str {
        self.name.as_deref().unwrap_or_default()
    }
}

/// One tool call a model asked for, as the wire reported it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatToolCall {
    /// The provider's handle for this call; the matching result has to quote it.
    pub id: String,
    /// Which tool to run.
    pub name: String,
    /// The arguments, parsed. A provider that sends malformed JSON yields an empty object, and
    /// the tool's own validator refuses it with `bad_arguments` — a broken argument blob from a
    /// model is an expected outcome, not a transport error.
    #[serde(default)]
    pub arguments: Value,
}

/// One chat exchange, addressed to a model.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// Wire key of the model the router resolved.
    pub model: String,
    /// Conversation so far.
    pub messages: Vec<ChatMessage>,
    /// Sampling temperature, when the caller sets one.
    pub temperature: Option<f64>,
    /// Answer budget in tokens, when the caller sets one.
    pub max_tokens: Option<u32>,
    /// Tools the model may call. Empty means a plain conversation: the adapters then omit the
    /// field entirely rather than sending an empty list, which some providers answer with a 400.
    pub tools: Vec<ToolSpec>,
}

impl ChatRequest {
    /// A plain request to one model, with no tools offered.
    #[must_use]
    pub fn new(model: impl Into<String>, messages: Vec<ChatMessage>) -> Self {
        Self {
            model: model.into(),
            messages,
            temperature: None,
            max_tokens: None,
            tools: Vec::new(),
        }
    }

    /// The same request with a tool list attached.
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<ToolSpec>) -> Self {
        self.tools = tools;
        self
    }
}

/// Token counts a provider reported.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatUsage {
    /// Input tokens.
    pub prompt_tokens: Option<u64>,
    /// Output tokens.
    pub completion_tokens: Option<u64>,
    /// Total tokens (providers disagree whether it includes both; reported as-is).
    pub total_tokens: Option<u64>,
}

/// A finished answer.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatOutcome {
    /// The answer text.
    pub content: String,
    /// Why the model stopped, as the provider put it.
    pub finish_reason: Option<String>,
    /// Token counts, when the provider reported them.
    pub usage: Option<ChatUsage>,
    /// Tool calls the model asked for. A non-empty list means this turn is *not* the answer:
    /// the results have to go back to the model before it can finish.
    pub tool_calls: Vec<ChatToolCall>,
}

impl ChatOutcome {
    /// A plain answer: text, no tool calls, nothing claimed about tokens.
    #[must_use]
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            finish_reason: None,
            usage: None,
            tool_calls: Vec::new(),
        }
    }

    /// Whether the provider considered this turn finished.
    ///
    /// Providers disagree on what "finished" means — OpenAI says `stop` for a final answer and
    /// `tool_calls` when it wants tools, Anthropic says `end_turn` and `tool_use` — so the
    /// check is on the *absence* of tool calls. A turn that asked for tools is never final,
    /// whatever the reason string says.
    #[must_use]
    pub fn is_final(&self) -> bool {
        self.tool_calls.is_empty()
    }
}

/// What the client pushes while an answer streams in.
#[derive(Debug, Clone)]
pub enum ChatEvent {
    /// The exchange is about to start; carries what the router resolved.
    Start {
        /// Provider display name.
        provider: String,
        /// Wire key of the model.
        model: String,
        /// Wire protocol.
        protocol: String,
    },
    /// A piece of the answer, in arrival order.
    Delta(String),
}

/// Shared HTTP client: connections are pooled across providers and calls.
fn http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .expect("the AI Hub HTTP client must build")
    })
}

/// Check a request before anything is sent: an empty conversation or a blank model is a bug in
/// the caller, not something to ask a provider about.
pub fn validate_request(request: &ChatRequest) -> Result<()> {
    if request.model.trim().is_empty() {
        return Err(AiHubError::InvalidChatRequest(
            "a chat request needs a model".to_owned(),
        ));
    }
    if request.messages.is_empty() {
        return Err(AiHubError::InvalidChatRequest(
            "a chat request needs at least one message".to_owned(),
        ));
    }
    if request
        .messages
        .iter()
        // An assistant turn that was *only* a tool call carries its meaning in
        // `tool_calls`, and its text is legitimately empty — every protocol accepts that
        // (OpenAI wants `content: null`). Refusing it here would make a tool-using turn the
        // one request the platform rejects itself, which is how a run that works looks broken.
        .any(|message| {
            message.content.trim().is_empty()
                && !(message.role == ChatRole::Assistant && !message.tool_calls.is_empty())
        })
    {
        return Err(AiHubError::InvalidChatRequest(
            "chat messages may not be empty".to_owned(),
        ));
    }
    // A tool result has to answer a call, or the provider has nothing to pair it with and
    // refuses the whole request. Checking it here names the problem before it costs a call.
    if request
        .messages
        .iter()
        .any(|message| message.role == ChatRole::Tool && message.call_id().is_empty())
    {
        return Err(AiHubError::InvalidChatRequest(
            "a tool result must quote the call it answers".to_owned(),
        ));
    }
    if let Some(temperature) = request.temperature
        && !(0.0..=2.0).contains(&temperature)
    {
        return Err(AiHubError::InvalidChatRequest(
            "temperature must be between 0 and 2".to_owned(),
        ));
    }
    if let Some(max_tokens) = request.max_tokens
        && (max_tokens == 0 || max_tokens > 100_000)
    {
        return Err(AiHubError::InvalidChatRequest(
            "max_tokens must be between 1 and 100000".to_owned(),
        ));
    }

    Ok(())
}

/// A conversation, answered in one piece.
///
/// The path, the headers, the body and the reading of the answer all come from the target's
/// protocol adapter, so a call is made the same way whichever vendor answers.
pub async fn chat(target: &ProviderTarget, request: &ChatRequest) -> Result<ChatOutcome> {
    validate_request(request)?;
    let adapter = adapter_for(&target.protocol);

    let response = http()
        .post(target.endpoint(&adapter.chat_path(&request.model, false)))
        .headers(adapter.auth_headers(target.api_key.as_deref()))
        .json(&adapter.build_chat(request, false))
        .timeout(Duration::from_millis(target.timeout_ms.max(1)))
        .send()
        .await
        .map_err(|error| AiHubError::Transport(error.to_string()))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|error| AiHubError::Transport(error.to_string()))?;

    if !status.is_success() {
        return Err(AiHubError::Upstream {
            status: status.as_u16(),
            message: trimmed(&text),
        });
    }

    let value: Value = serde_json::from_str(&text)
        .map_err(|error| AiHubError::Malformed(format!("{error}: {}", trimmed(&text))))?;
    refused(&value)?;

    let outcome = adapter.parse_answer(&value).ok_or_else(|| {
        AiHubError::Malformed(format!("no answer in the response: {}", trimmed(&text)))
    })?;

    Ok(outcome)
}

/// A conversation, streamed.
///
/// Deltas and the start frame are pushed onto `events` in arrival order; the returned outcome
/// carries the whole answer once the stream ends, so a caller can store it after showing it.
/// A caller that stops reading (a closed channel) ends the stream early without an error.
pub async fn stream_chat(
    target: &ProviderTarget,
    request: &ChatRequest,
    events: &mpsc::Sender<ChatEvent>,
) -> Result<ChatOutcome> {
    validate_request(request)?;
    let adapter = adapter_for(&target.protocol);

    let mut response = http()
        .post(target.endpoint(&adapter.chat_path(&request.model, true)))
        .headers(adapter.auth_headers(target.api_key.as_deref()))
        .json(&adapter.build_chat(request, true))
        .send()
        .await
        .map_err(|error| AiHubError::Transport(error.to_string()))?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        return Err(AiHubError::Upstream {
            status,
            message: trimmed(&text),
        });
    }

    let start = ChatEvent::Start {
        provider: target.name.clone(),
        model: request.model.clone(),
        protocol: target.protocol.clone(),
    };
    let _ = events.send(start).await;

    let mut parser = StreamParser::new(adapter.decoder());
    let mut outcome = ChatOutcome {
        content: String::new(),
        finish_reason: None,
        usage: None,
        tool_calls: Vec::new(),
    };
    // Tool calls are merged as they stream, keyed by the index the provider used, and only
    // turned into whole calls once the stream ends — a call's arguments are not valid JSON
    // until the last fragment has arrived.
    let mut calls = ToolCallAssembler::default();

    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| AiHubError::Stream(error.to_string()))?
    {
        for piece in parser.push(&String::from_utf8_lossy(&chunk)) {
            if let Err(error) = absorb(piece?, &mut outcome, &mut calls, &events).await {
                if error.to_string() == STREAM_ABANDONED {
                    // The caller stopped listening: the answer in hand is still the answer.
                    outcome.tool_calls = calls.finish(|position| position as u32);
                    return Ok(outcome);
                }
                return Err(error);
            }
        }
    }

    let mut abandoned = false;
    for piece in parser.finish() {
        if let Err(error) = absorb(piece?, &mut outcome, &mut calls, &events).await {
            if error.to_string() == STREAM_ABANDONED {
                abandoned = true;
            } else {
                return Err(error);
            }
        }
    }
    outcome.tool_calls = calls.finish(|position| position as u32);
    if abandoned {
        return Ok(outcome);
    }

    // A turn that only asked for tools carries no text and, on some providers, no finish reason
    // either — it is a perfectly good turn. Only a turn with *nothing* in it is malformed.
    if outcome.content.is_empty() && outcome.finish_reason.is_none() && outcome.tool_calls.is_empty() {
        return Err(AiHubError::Malformed(
            "the provider streamed no answer and no finish reason".to_owned(),
        ));
    }

    Ok(outcome)
}

/// The provider's own model list.
pub async fn list_remote_models(target: &ProviderTarget) -> Result<Vec<String>> {
    let adapter = adapter_for(&target.protocol);
    let response = http()
        .get(target.endpoint(&adapter.models_path()))
        .headers(adapter.auth_headers(target.api_key.as_deref()))
        .timeout(Duration::from_millis(target.timeout_ms.max(1)))
        .send()
        .await
        .map_err(|error| AiHubError::Transport(error.to_string()))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|error| AiHubError::Transport(error.to_string()))?;

    if !status.is_success() {
        return Err(AiHubError::Upstream {
            status: status.as_u16(),
            message: trimmed(&text),
        });
    }

    let value: Value = serde_json::from_str(&text)
        .map_err(|error| AiHubError::Malformed(format!("{error}: {}", trimmed(&text))))?;
    refused(&value)?;

    // Each protocol names its list differently; the adapter knows which shape is which.
    let mut models: Vec<String> = adapter
        .parse_model_list(&value)
        .ok_or_else(|| {
            AiHubError::Malformed(format!("no model list in the response: {}", trimmed(&text)))
        })?
        .into_iter()
        .filter(|id| !id.trim().is_empty())
        .collect();
    models.sort();
    models.dedup();

    Ok(models)
}

/// A refusal reported inside an otherwise successful answer.
///
/// Shared with the OpenAI-compatible stream decoder, which meets the same shape mid-stream.
pub fn refused(value: &Value) -> Result<()> {
    let Some(error) = value.get("error") else {
        return Ok(());
    };
    if error.is_null() {
        return Ok(());
    }

    let message = error
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| trimmed(&error.to_string()));

    Err(AiHubError::Upstream {
        status: 200,
        message,
    })
}

/// Trim a body to something an operator can read in a log line.
///
/// Shared with the protocol adapters, so every vendor message is clipped the same way.
pub fn clip_body(body: &str) -> String {
    trimmed(body)
}

/// Trim a body to something an operator can read in a log line.
fn trimmed(body: &str) -> String {
    let body = body.trim();
    if body.chars().count() <= MAX_ERROR_BODY {
        return body.to_owned();
    }

    let trimmed: String = body.chars().take(MAX_ERROR_BODY).collect();
    format!("{trimmed}…")
}

/// Fold one decoded stream piece into the answer and push its text to the caller.
///
/// A text piece grows the answer (bounded — a runaway provider is refused, not buffered) and is
/// pushed onto the caller's channel; a finish reason and a usage block are remembered. This is
/// the single place where a vendor's frames become the platform's own stream, so every adapter
/// produces the same sequence: text, text, …, usage, done.
async fn absorb(
    piece: StreamPiece,
    outcome: &mut ChatOutcome,
    calls: &mut ToolCallAssembler,
    events: &mpsc::Sender<ChatEvent>,
) -> Result<()> {
    if let Some(text) = piece.content {
        if outcome.content.len() + text.len() > MAX_ANSWER_BYTES {
            return Err(AiHubError::Stream(
                "the provider sent more than the platform accepts for one answer".to_owned(),
            ));
        }
        outcome.content.push_str(&text);
        if events.send(ChatEvent::Delta(text)).await.is_err() {
            // The caller stopped listening. The stream is finished from its side; the answer in
            // hand is still the answer, and the outcome below is what the route records.
            return Err(AiHubError::Stream(STREAM_ABANDONED.to_owned()));
        }
    }
    if let Some(piece) = piece.tool_call
        && !calls.push(piece)
    {
        return Err(AiHubError::Stream(
            "the provider streamed more tool-call bytes than the platform accepts".to_owned(),
        ));
    }
    if piece.finish_reason.is_some() {
        outcome.finish_reason = piece.finish_reason;
    }
    if piece.usage.is_some() {
        outcome.usage = piece.usage;
    }

    Ok(())
}

/// Collects the tool calls of one streamed turn out of the fragments the provider sent.
///
/// A streamed tool call is not one frame. OpenAI sends the id and the name in the first delta
/// and the argument JSON a few characters at a time, Anthropic splits the same thing across
/// `content_block_start` and a run of `input_json_delta`s, and both interleave two calls in one
/// turn by index. The fragments are therefore keyed by index and the arguments are
/// *concatenated* — never parsed per fragment, because a JSON object is routinely split
/// mid-token (`{"city":"Is` + `tanbul"}` is the ordinary case, not a broken provider).
#[derive(Debug, Default)]
pub(crate) struct ToolCallAssembler {
    /// The calls found so far, in the order their ids first appeared.
    calls: Vec<ChatToolCall>,
    /// Argument bytes per call index, still incomplete.
    fragments: BTreeMap<u32, String>,
}

impl ToolCallAssembler {
    /// Merge one fragment. Returns `false` when a call's arguments outgrow the answer limit,
    /// which is a provider that never stops sending rather than one that is slow.
    pub(crate) fn push(&mut self, piece: crate::protocol::ToolCallPiece) -> bool {
        if let Some(fragment) = &piece.arguments_fragment {
            let buffer = self.fragments.entry(piece.index).or_default();
            buffer.push_str(fragment);
            if buffer.len() > MAX_ANSWER_BYTES {
                return false;
            }
        }

        match (&piece.id, &piece.name) {
            (Some(id), name) => {
                if let Some(position) = self.calls.iter().position(|call| &call.id == id) {
                    // The same id arriving twice is a provider repeating itself, not a second
                    // call: the name is refreshed and the argument buffer keeps accumulating.
                    if let Some(name) = name
                        && !name.is_empty()
                    {
                        self.calls[position].name.clone_from(name);
                    }
                } else {
                    self.calls.push(ChatToolCall {
                        id: id.clone(),
                        name: name.clone().unwrap_or_default(),
                        arguments: Value::Null,
                    });
                }
            }
            (None, Some(name)) => {
                // A protocol that names no id at all (Gemini) still identifies its call by name.
                if let Some(call) = self.calls.iter_mut().find(|call| call.name.is_empty()) {
                    call.name.clone_from(name);
                }
            }
            (None, None) => {}
        }

        true
    }

    /// The finished calls, with their arguments parsed.
    ///
    /// A call whose arguments never became parseable JSON yields an empty object rather than
    /// being dropped: the tool's own validator then refuses it with a message the model can
    /// read and correct, which is a better outcome than a turn that silently lost a tool call.
    #[must_use]
    pub(crate) fn finish(self, fallback_index: impl Fn(usize) -> u32) -> Vec<ChatToolCall> {
        self.calls
            .into_iter()
            .enumerate()
            .map(|(position, mut call)| {
                let raw = self
                    .fragments
                    .get(&fallback_index(position))
                    .or_else(|| self.fragments.values().next())
                    .cloned()
                    .unwrap_or_default();
                call.arguments = serde_json::from_str(&raw)
                    .ok()
                    .filter(Value::is_object)
                    .unwrap_or_else(|| json!({}));
                call
            })
            .collect()
    }
}

/// Marker error a closed subscriber channel produces; it ends the stream without a failure.
const STREAM_ABANDONED: &str = "__omnion_stream_abandoned__";

/// Framing of a `text/event-stream` body, with the payload decoding left to the protocol adapter.
///
/// It is deliberately forgiving: providers wrap frames differently (`\n\n` vs `\r\n\r\n`), send
/// keep-alive comments, omit the trailing blank line, and disagree about which frame carries the
/// end sentinel — OpenAI sends `[DONE]`, the messages protocol sends a `message_stop` event and
/// Gemini simply stops after a finish reason. The decoder holds the incomplete tail of a frame
/// until the rest of it arrives, because a chunk boundary in the middle of a JSON object must
/// never lose an answer.
struct StreamParser {
    decoder: Box<dyn crate::protocol::StreamDecoder>,
    buffer: String,
    done: bool,
}

impl StreamParser {
    /// A parser for one protocol's frames.
    fn new(decoder: Box<dyn crate::protocol::StreamDecoder>) -> Self {
        Self {
            decoder,
            buffer: String::new(),
            done: false,
        }
    }

    /// Feed one chunk; every complete frame it decodes comes back as a piece.
    fn push(&mut self, chunk: &str) -> Vec<Result<StreamPiece>> {
        self.buffer.push_str(&chunk.replace("\r\n", "\n"));

        let mut pieces = Vec::new();
        while let Some(index) = self.buffer.find("\n\n") {
            let frame: String = self.buffer.drain(..index + 2).collect();
            if let Some(piece) = self.decode(&frame) {
                pieces.push(piece);
            }
            if self.done {
                break;
            }
        }

        pieces
    }

    /// Flush a trailing frame that arrived without its closing blank line.
    fn finish(&mut self) -> Vec<Result<StreamPiece>> {
        if self.done || self.buffer.trim().is_empty() {
            return Vec::new();
        }

        let frame = std::mem::take(&mut self.buffer);
        self.decode(&frame).into_iter().collect()
    }

    /// Decode one frame: the concatenated `data:` lines of one event.
    fn decode(&mut self, frame: &str) -> Option<Result<StreamPiece>> {
        let mut data = String::new();
        for line in frame.lines() {
            let line = line.trim_end();
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("data:") {
                data.push_str(rest.trim_start());
            }
        }

        if data.is_empty() {
            return None;
        }

        match self.decoder.decode(&data) {
            Ok(piece) => {
                if self.decoder.is_done() {
                    self.done = true;
                }
                if piece.is_empty() {
                    return None;
                }
                Some(Ok(piece))
            }
            Err(error) => Some(Err(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A framing parser over the OpenAI-compatible decoder — the protocol most tests speak.
    fn openai_parser() -> StreamParser {
        StreamParser::new(crate::protocol::adapter_for("openai_compatible").decoder())
    }

    fn target() -> ProviderTarget {
        ProviderTarget {
            id: Uuid::nil(),
            name: "Mock".to_owned(),
            protocol: "openai_compatible".to_owned(),
            base_url: "https://api.example.com/v1/".to_owned(),
            api_key: Some("sk-test".to_owned()),
            timeout_ms: 30_000,
        }
    }

    #[test]
    fn targets_build_endpoints_without_a_double_slash() {
        assert_eq!(
            target().endpoint("/chat/completions"),
            "https://api.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn targets_take_the_stored_provider() {
        let provider = Provider {
            id: Uuid::nil(),
            name: "Office".to_owned(),
            protocol: "openai_compatible".to_owned(),
            kind: "local".to_owned(),
            base_url: "http://127.0.0.1:11434/v1/".to_owned(),
            api_key: Some(String::new()),
            timeout_ms: 30_000,
            max_retries: 1,
            priority: 100,
            last_health: "unknown".to_owned(),
            last_checked_at: None,
            last_error: None,
            enabled: true,
            is_default: false,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        let target = ProviderTarget::from_provider(&provider);
        assert_eq!(target.base_url, "http://127.0.0.1:11434/v1");
        assert_eq!(target.api_key, None, "an empty stored key is no key");
    }

    #[test]
    fn roles_round_trip_on_the_wire() {
        for role in [ChatRole::System, ChatRole::User, ChatRole::Assistant] {
            assert_eq!(ChatRole::parse(role.as_str()).expect("known role"), role);
        }
        assert!(ChatRole::parse("robot").is_err());
    }

    #[test]
    fn requests_are_validated_before_anything_is_sent() {
        let request = ChatRequest {
            model: "mock-small".to_owned(),
            messages: vec![ChatMessage::user("hello")],
            temperature: None,
            max_tokens: None,
            tools: Vec::new(),
        };
        assert!(validate_request(&request).is_ok());

        let no_model = ChatRequest {
            model: " ".to_owned(),
            ..request.clone()
        };
        assert!(matches!(
            validate_request(&no_model),
            Err(AiHubError::InvalidChatRequest(_))
        ));

        let no_messages = ChatRequest {
            messages: Vec::new(),
            ..request.clone()
        };
        assert!(validate_request(&no_messages).is_err());

        let empty_message = ChatRequest {
            messages: vec![ChatMessage::user("   ")],
            ..request.clone()
        };
        assert!(validate_request(&empty_message).is_err());

        let hot = ChatRequest {
            temperature: Some(9.0),
            ..request.clone()
        };
        assert!(validate_request(&hot).is_err());

        let huge = ChatRequest {
            max_tokens: Some(0),
            ..request
        };
        assert!(validate_request(&huge).is_err());
    }

    #[test]
    fn the_parser_reads_openai_frames() {
        let mut parser = openai_parser();
        let pieces = parser.push(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\n\n",
        );
        assert_eq!(pieces.len(), 2);
        assert_eq!(
            pieces[0].as_ref().expect("first frame").content.as_deref(),
            Some("Hel")
        );
        let second = pieces[1].as_ref().expect("second frame");
        assert_eq!(second.content.as_deref(), Some("lo"));
        assert_eq!(second.finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn the_parser_survives_a_split_frame() {
        let mut parser = openai_parser();
        assert!(parser.push("data: {\"choices\":[{\"delt").is_empty());
        let pieces = parser.push("a\":{\"content\":\"hi\"}}]}\n\n");
        assert_eq!(pieces.len(), 1);
        assert_eq!(
            pieces[0].as_ref().expect("frame").content.as_deref(),
            Some("hi")
        );
    }

    #[test]
    fn the_parser_handles_crlf_keep_alives_and_the_done_sentinel() {
        let mut parser = openai_parser();
        let pieces = parser.push(
            ": keep-alive\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\r\n\r\ndata: [DONE]\r\n\r\n",
        );
        assert_eq!(pieces.len(), 1);
        assert!(parser.done);

        let mut trailing = openai_parser();
        assert!(
            trailing
                .push("data: {\"choices\":[{\"delta\":{\"content\":\"z\"}}]}")
                .is_empty()
        );
        let flushed = trailing.finish();
        assert_eq!(
            flushed[0].as_ref().expect("flushed").content.as_deref(),
            Some("z")
        );
    }

    #[test]
    fn the_parser_reports_a_usage_frame() {
        let mut parser = openai_parser();
        let pieces = parser.push(
            "data: {\"choices\":[{\"delta\":{\"content\":\"\"},\"finish_reason\":\"stop\"}],\
             \"usage\":{\"prompt_tokens\":8,\"completion_tokens\":3,\"total_tokens\":11}}\n\n",
        );
        let piece = pieces[0].as_ref().expect("frame");
        let usage = piece.usage.as_ref().expect("usage");
        assert_eq!(usage.prompt_tokens, Some(8));
        assert_eq!(usage.total_tokens, Some(11));
    }

    #[test]
    fn the_parser_reports_a_refusal_inside_a_stream() {
        let mut parser = openai_parser();
        let pieces = parser.push("data: {\"error\":{\"message\":\"model is offline\"}}\n\n");
        let error = pieces[0].as_ref().expect_err("refusal");
        assert!(matches!(error, AiHubError::Upstream { status: 200, .. }));
        assert!(error.to_string().contains("model is offline"));
    }

    #[test]
    fn a_non_streaming_answer_is_read_through_the_protocol_layer() {
        let value: Value = serde_json::from_str(
            r#"{"choices":[{"message":{"role":"assistant","content":"Hello"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":5,"completion_tokens":1,"total_tokens":6}}"#,
        )
        .expect("a body");
        let outcome = crate::protocol::adapter_for("openai_compatible")
            .parse_answer(&value)
            .expect("an answer");
        assert_eq!(outcome.content, "Hello");
        assert_eq!(outcome.finish_reason.as_deref(), Some("stop"));
        assert_eq!(outcome.usage.expect("usage").total_tokens, Some(6));
    }

    #[test]
    fn content_parts_are_joined() {
        let value: Value = serde_json::from_str(
            r#"{"choices":[{"message":{"content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}}]}"#,
        )
        .expect("a body");
        assert_eq!(
            crate::protocol::adapter_for("openai_compatible")
                .parse_answer(&value)
                .expect("answer")
                .content,
            "ab"
        );
    }

    #[test]
    fn a_refusal_body_is_recognised() {
        let value = json!({"error": {"message": "no such model", "type": "invalid_request_error"}});
        assert!(refused(&value).is_err());
        assert!(refused(&json!({"error": null})).is_ok());
        assert!(refused(&json!({"choices": []})).is_ok());
    }

    #[test]
    fn long_bodies_are_trimmed() {
        let long = "x".repeat(MAX_ERROR_BODY + 50);
        let trimmed = trimmed(&long);
        assert_eq!(trimmed.chars().count(), MAX_ERROR_BODY + 1);
        assert!(trimmed.ends_with('…'));
    }
}
