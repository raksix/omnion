//! Protocol adapters: the wire a provider speaks, behind one small trait.
//!
//! v0 spoke the OpenAI-compatible protocol and nothing else. This module turns that into a
//! runtime: three adapters implement [`ProtocolAdapter`], a fourth attaches through the trait
//! without touching the router, and everything above the client keeps calling
//! [`crate::client::chat`] and [`crate::client::stream_chat`] without learning which vendor
//! answered (docs/requests/REQ-097 § "Protocol adapters").
//!
//! An adapter owns four decisions and nothing else:
//!
//! * **where** a call goes — [`ProtocolAdapter::chat_path`], [`ProtocolAdapter::models_path`];
//! * **how it is authenticated** — [`ProtocolAdapter::auth_headers`];
//! * **what a request body looks like** — [`ProtocolAdapter::build_chat`];
//! * **how an answer comes back** — [`ProtocolAdapter::parse_answer`], and for a stream
//!   [`ProtocolAdapter::decoder`].
//!
//! The normalised output is the same for all three: a [`ChatOutcome`] with text, a finish reason
//! and token counts, and for a stream a sequence of [`StreamPiece`]s whose `content`,
//! `finish_reason` and `usage` are vendor-free. A stream that ends without usage records
//! `None` rather than inventing counts.

use serde_json::{Value, json};

use crate::client::{ChatMessage, ChatOutcome, ChatRequest, ChatUsage};
use crate::error::{AiHubError, Result};

/// One decoded piece of a provider's answer, whatever the vendor sent.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamPiece {
    /// A piece of the answer, when this frame carried one.
    pub content: Option<String>,
    /// Why the model stopped, when the provider said so in this frame.
    pub finish_reason: Option<String>,
    /// Token counts, when the provider reported them in this frame.
    pub usage: Option<ChatUsage>,
    /// One piece of a tool call, when this frame carried part of one.
    ///
    /// A streamed tool call is *not* one frame: OpenAI sends the name and the id in the first
    /// delta and the argument JSON in a string fragment that may itself be split across frames,
    /// and providers interleave calls by index. The piece therefore carries only what this frame
    /// added, and [`crate::client::ToolCallAssembler`] merges pieces into whole calls.
    pub tool_call: Option<ToolCallPiece>,
}

impl Default for StreamPiece {
    fn default() -> Self {
        Self {
            content: None,
            finish_reason: None,
            usage: None,
            tool_call: None,
        }
    }
}

impl StreamPiece {
    /// `true` when the piece carries nothing a subscriber can see.
    ///
    /// A tool-call piece is not empty: the run has to know the model wants a tool even though
    /// there is no text to stream.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.content.is_none()
            && self.finish_reason.is_none()
            && self.usage.is_none()
            && self.tool_call.is_none()
    }
}

/// One fragment of a tool call, as one stream frame carried it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolCallPiece {
    /// Position in the turn's call list, which is how interleaved calls are told apart.
    pub index: u32,
    /// The provider's handle, when this frame carried it.
    pub id: Option<String>,
    /// The tool name, when this frame carried it.
    pub name: Option<String>,
    /// A fragment of the argument JSON, when this frame carried one.
    pub arguments_fragment: Option<String>,
}

impl ToolCallPiece {
    /// A fragment carrying only arguments, the common case after the first frame.
    #[must_use]
    pub fn arguments(index: u32, fragment: impl Into<String>) -> Self {
        Self {
            index,
            id: None,
            name: None,
            arguments_fragment: Some(fragment.into()),
        }
    }
}

/// Decoder of one vendor's stream frames.
///
/// The framing itself (SSE boundaries, keep-alive comments, split frames) is handled once by
/// [`crate::client`]; an adapter only turns one `data:` payload into a [`StreamPiece`].
pub trait StreamDecoder: Send {
    /// Decode one frame payload. An `Err` ends the stream with a provider error.
    fn decode(&mut self, data: &str) -> Result<StreamPiece>;
    /// `true` once the provider sent its end-of-stream sentinel.
    fn is_done(&self) -> bool {
        false
    }
}

/// The wire protocol of one provider.
pub trait ProtocolAdapter: Send + Sync {
    /// Protocol key, as it is stored (`openai_compatible`, …).
    fn protocol(&self) -> &'static str;

    /// Human note the panel shows next to the protocol in the form.
    fn capability_note(&self) -> &'static str;

    /// Path of one chat call, appended to the provider's base URL.
    ///
    /// `stream` is part of the question because Gemini addresses its streaming operation with a
    /// different path (`:streamGenerateContent`) rather than a flag in the body.
    fn chat_path(&self, model: &str, stream: bool) -> String;

    /// Path of the provider's own model list.
    fn models_path(&self) -> String;

    /// How this protocol authenticates.
    fn auth_headers(&self, api_key: Option<&str>) -> reqwest::header::HeaderMap;

    /// The JSON body of one chat call.
    fn build_chat(&self, request: &ChatRequest, stream: bool) -> Value;

    /// Read a non-streaming answer, or `None` when the body carries none.
    fn parse_answer(&self, value: &Value) -> Option<ChatOutcome>;

    /// Read the provider's model list, or `None` when the body carries none.
    fn parse_model_list(&self, value: &Value) -> Option<Vec<String>>;

    /// A decoder for this protocol's stream frames.
    fn decoder(&self) -> Box<dyn StreamDecoder>;
}

/// One adapter in the registry.
static OPENAI: OpenAiCompatible = OpenAiCompatible;
static ANTHROPIC: AnthropicMessages = AnthropicMessages;
static GEMINI: GoogleGemini = GoogleGemini;

/// The adapter for a stored protocol key, or the default one for an unknown key.
///
/// An unknown protocol never reaches a call — the store and the API refuse it first — so the
/// fallback here is only the seam a fourth adapter fills without touching callers.
#[must_use]
pub fn adapter_for(protocol: &str) -> &'static dyn ProtocolAdapter {
    match protocol {
        "anthropic_messages" => &ANTHROPIC,
        "google_gemini" => &GEMINI,
        _ => &OPENAI,
    }
}

/// What the panel shows about one protocol, and the shape the API answers with.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProtocolInfo {
    /// Protocol key as it is stored and sent.
    pub protocol: &'static str,
    /// One line about what the protocol covers.
    pub note: &'static str,
    /// Where a call goes, relative to the base URL.
    pub chat_path: &'static str,
    /// How the key is sent.
    pub auth: &'static str,
}

const PROTOCOL_INFOS: &[ProtocolInfo] = &[
    ProtocolInfo {
        protocol: "openai_compatible",
        note: "Chat completions. Every hosted API and every local OpenAI-compatible server \
               (Ollama, vLLM, llama.cpp) speaks it.",
        chat_path: "/chat/completions",
        auth: "Authorization: Bearer <key>",
    },
    ProtocolInfo {
        protocol: "anthropic_messages",
        note: "The messages wire shape: a system block outside the conversation, token counts \
               under input/output names.",
        chat_path: "/messages",
        auth: "x-api-key: <key> + anthropic-version",
    },
    ProtocolInfo {
        protocol: "google_gemini",
        note: "generateContent: contents with roles user/model, usage under usageMetadata.",
        chat_path: "/models/{model}:generateContent",
        auth: "x-goog-api-key: <key>",
    },
];

/// The protocols the panel offers, in the order the form lists them.
#[must_use]
pub fn protocol_infos() -> &'static [ProtocolInfo] {
    PROTOCOL_INFOS
}

// ---------------------------------------------------------------------------------------------
// OpenAI-compatible
// ---------------------------------------------------------------------------------------------

/// The protocol v0 already spoke, and the one every local OpenAI-compatible server exposes.
struct OpenAiCompatible;

impl ProtocolAdapter for OpenAiCompatible {
    fn protocol(&self) -> &'static str {
        "openai_compatible"
    }

    fn capability_note(&self) -> &'static str {
        "Chat completions with a bearer key; the default for local servers."
    }

    fn chat_path(&self, _model: &str, _stream: bool) -> String {
        "/chat/completions".to_owned()
    }

    fn models_path(&self) -> String {
        "/models".to_owned()
    }

    fn auth_headers(&self, api_key: Option<&str>) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(key) = api_key.filter(|key| !key.is_empty())
            && let Ok(value) = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))
        {
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
        headers
    }

    fn build_chat(&self, request: &ChatRequest, stream: bool) -> Value {
        let mut body = json!({
            "model": request.model,
            "messages": request.messages.iter().map(openai_message).collect::<Vec<_>>(),
            "stream": stream,
        });
        if stream {
            // A streamed answer carries no token counts unless the caller asks for them: OpenAI,
            // vLLM and llama.cpp all read `stream_options.include_usage` and report nothing when
            // it is absent, so without this the Usage tab can only ever say "unknown" for every
            // local endpoint — a stream that ends without a usage frame is honest, but one that
            // was never asked is a platform omission, not a provider's.
            body["stream_options"] = json!({ "include_usage": true });
        }
        if let Some(temperature) = request.temperature {
            body["temperature"] = json!(temperature);
        }
        if let Some(max_tokens) = request.max_tokens {
            body["max_tokens"] = json!(max_tokens);
        }
        if !request.tools.is_empty() {
            body["tools"] = json!(request
                .tools
                .iter()
                .map(|tool| json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": if tool.parameters.is_null() {
                            json!({ "type": "object", "properties": {} })
                        } else {
                            tool.parameters.clone()
                        },
                    }
                }))
                .collect::<Vec<_>>());
            // Without this the model may answer in prose instead of calling anything, and a
            // tool-capable model that skips the flag is the single most common cause of "the
            // agent never used its tools".
            body["tool_choice"] = json!("auto");
        }
        body
    }

    fn parse_answer(&self, value: &Value) -> Option<ChatOutcome> {
        let choice = value.get("choices")?.as_array()?.first()?;
        let content = choice
            .pointer("/message/content")
            .and_then(content_text)
            .unwrap_or_default();
        let finish_reason = choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let usage = value
            .get("usage")
            .and_then(|usage| serde_json::from_value::<ChatUsage>(usage.clone()).ok());

        Some(ChatOutcome {
            content,
            finish_reason,
            usage,
            tool_calls: read_openai_tool_calls(choice.pointer("/message/tool_calls")),
        })
    }

    fn parse_model_list(&self, value: &Value) -> Option<Vec<String>> {
        // OpenAI answers `{"data":[{"id":"…"}]}`; a few local runtimes answer a bare list.
        let entries = value
            .get("data")
            .and_then(Value::as_array)
            .or_else(|| value.as_array())?;
        Some(
            entries
                .iter()
                .filter_map(|entry| {
                    entry
                        .get("id")
                        .and_then(Value::as_str)
                        .or_else(|| entry.as_str())
                        .map(str::to_owned)
                })
                .collect(),
        )
    }

    fn decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(OpenAiStream { done: false })
    }
}

/// One OpenAI message, in the shape the chat endpoint accepts.
///
/// A tool result is a `role: "tool"` message that quotes the call it answers, and the tool
/// message *must* come immediately after the assistant turn that asked for it — a model
/// rejects the whole request if the pairing is broken, so a dropped result is a broken request
/// rather than a missing fact.
fn openai_message(message: &crate::client::ChatMessage) -> Value {
    use crate::client::ChatRole;

    match message.role {
        ChatRole::Tool => json!({
            "role": "tool",
            "tool_call_id": message.call_id(),
            "content": message.content,
        }),
        ChatRole::Assistant if !message.tool_calls.is_empty() => json!({
            "role": "assistant",
            "content": if message.content.is_empty() { Value::Null } else { json!(message.content) },
            "tool_calls": message
                .tool_calls
                .iter()
                .map(|call| json!({
                    "id": call.id,
                    "type": "function",
                    "function": { "name": call.name, "arguments": call.arguments.to_string() },
                }))
                .collect::<Vec<_>>(),
        }),
        ChatRole::Assistant if message.content.is_empty() => {
            // An assistant turn that was only a tool call has no text; sending an empty string
            // is legal, but some providers prefer null. Either is accepted, null is tidier.
            json!({ "role": "assistant", "content": Value::Null })
        }
        _ => json!({ "role": message.role.as_str(), "content": message.content }),
    }
}

/// Tool calls as the OpenAI chat endpoint reports them.
fn read_openai_tool_calls(value: Option<&Value>) -> Vec<crate::client::ChatToolCall> {
    let Some(entries) = value.and_then(Value::as_array) else {
        return Vec::new();
    };

    entries
        .iter()
        .filter_map(|entry| {
            let function = entry.get("function")?;
            let name = function.get("name").and_then(Value::as_str)?;
            let arguments = match function.get("arguments") {
                // Providers send the arguments as a JSON *string*, and a small number of them
                // send an object. Both are read; anything that is not parseable becomes an
                // empty object so the tool's own validator refuses it with a message instead
                // of the transport reporting a parse error nobody can act on.
                Some(Value::String(raw)) => serde_json::from_str(raw).unwrap_or_else(|_| json!({})),
                Some(object) if object.is_object() => object.clone(),
                _ => json!({}),
            };
            Some(crate::client::ChatToolCall {
                id: entry
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                name: name.to_owned(),
                arguments,
            })
        })
        .collect()
}

/// Decoder of an OpenAI-compatible stream.
struct OpenAiStream {
    done: bool,
}

impl StreamDecoder for OpenAiStream {
    fn decode(&mut self, data: &str) -> Result<StreamPiece> {
        if data.trim() == "[DONE]" {
            self.done = true;
            return Ok(StreamPiece::default());
        }

        let value: Value = serde_json::from_str(data)
            .map_err(|error| AiHubError::Stream(format!("{error}: {}", clip(data))))?;
        crate::client::refused(&value)?;

        let mut piece = StreamPiece::default();
        if let Some(choice) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        {
            if let Some(content) = choice.pointer("/delta/content").and_then(content_text)
                && !content.is_empty()
            {
                piece.content = Some(content);
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                piece.finish_reason = Some(reason.to_owned());
            }
            piece.tool_call = choice
                .pointer("/delta/tool_calls")
                .and_then(|calls| calls.as_array().and_then(|calls| calls.first()))
                .and_then(|call| {
                    let function = call.get("function")?;
                    let fragment = function.get("arguments").and_then(Value::as_str);
                    let id = call.get("id").and_then(Value::as_str);
                    let name = function.get("name").and_then(Value::as_str);
                    if id.is_none() && name.is_none() && fragment.is_none() {
                        return None;
                    }
                    Some(ToolCallPiece {
                        index: call.get("index").and_then(Value::as_u64).unwrap_or(0) as u32,
                        id: id.map(str::to_owned),
                        name: name.map(str::to_owned),
                        arguments_fragment: fragment.map(str::to_owned),
                    })
                });
        }
        if let Some(usage) = value.get("usage") {
            piece.usage = serde_json::from_value::<ChatUsage>(usage.clone()).ok();
        }

        Ok(piece)
    }

    fn is_done(&self) -> bool {
        self.done
    }
}

// ---------------------------------------------------------------------------------------------
// Anthropic messages
// ---------------------------------------------------------------------------------------------

/// The messages wire shape: a system block outside the conversation, `input_tokens` /
/// `output_tokens` for the counts, and typed stream events instead of OpenAI's `choices`.
struct AnthropicMessages;

/// The API version this adapter speaks.
const ANTHROPIC_VERSION: &str = "2023-06-01";

impl ProtocolAdapter for AnthropicMessages {
    fn protocol(&self) -> &'static str {
        "anthropic_messages"
    }

    fn capability_note(&self) -> &'static str {
        "Messages API: system instructions sit outside the conversation; token counts are \
         input/output tokens."
    }

    fn chat_path(&self, _model: &str, _stream: bool) -> String {
        "/messages".to_owned()
    }

    fn models_path(&self) -> String {
        "/models".to_owned()
    }

    fn auth_headers(&self, api_key: Option<&str>) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(key) = api_key.filter(|key| !key.is_empty()) {
            if let Ok(value) = reqwest::header::HeaderValue::from_str(key) {
                headers.insert("x-api-key", value);
            }
        }
        if let Ok(value) = reqwest::header::HeaderValue::from_str(ANTHROPIC_VERSION) {
            headers.insert("anthropic-version", value);
        }
        headers.insert(
            "content-type",
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        headers
    }

    fn build_chat(&self, request: &ChatRequest, stream: bool) -> Value {
        let system: Vec<&str> = request
            .messages
            .iter()
            .filter(|message| message.role == crate::client::ChatRole::System)
            .map(|message| message.content.as_str())
            .collect();

        // The messages protocol keeps instructions outside the conversation and content as a
        // *list of blocks*. A turn that asked for tools and the turn that answers it are
        // different turns with different roles — an assistant turn holding `tool_use` blocks
        // and a user turn holding `tool_result` blocks. Merging them into one turn is the
        // mistake this shape exists to prevent: the provider sees a result with no call.
        //
        // So the blocks are collected with the role they belong to, and a role change flushes.
        let mut blocks: Vec<Value> = Vec::new();
        let mut block_role = String::new();
        let mut messages: Vec<Value> = Vec::new();
        let mut flush = |blocks: &mut Vec<Value>, role: &mut String, messages: &mut Vec<Value>| {
            if !blocks.is_empty() {
                messages.push(json!({ "role": role, "content": blocks }));
                blocks.clear();
            }
        };
        for message in request
            .messages
            .iter()
            .filter(|message| message.role != crate::client::ChatRole::System)
        {
            use crate::client::ChatRole;
            match message.role {
                ChatRole::Tool => {
                    if block_role != "user" {
                        flush(&mut blocks, &mut block_role, &mut messages);
                        block_role = "user".to_owned();
                    }
                    blocks.push(json!({
                        "type": "tool_result",
                        "tool_use_id": message.call_id(),
                        "content": message.content,
                        // A failed tool is a fact the model must reason about, not a transport
                        // error: without this flag Anthropic hands the result back as a success
                        // and the model retries the same failing call forever.
                        "is_error": false,
                    }));
                }
                ChatRole::Assistant if !message.tool_calls.is_empty() => {
                    if block_role != "assistant" {
                        flush(&mut blocks, &mut block_role, &mut messages);
                        block_role = "assistant".to_owned();
                    }
                    // One assistant turn can ask for several tools, and the messages protocol
                    // wants each as its own `tool_use` block on that turn.
                    for call in &message.tool_calls {
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": call.id,
                            "name": call.name,
                            "input": call.arguments,
                        }));
                    }
                }
                _ => {
                    flush(&mut blocks, &mut block_role, &mut messages);
                    let role = if message.role == ChatRole::Assistant {
                        "assistant"
                    } else {
                        "user"
                    };
                    messages.push(json!({
                        "role": role,
                        "content": [{ "type": "text", "text": message.content }],
                    }));
                }
            }
        }
        flush(&mut blocks, &mut block_role, &mut messages);

        // The protocol requires an answer budget, so a caller that set none gets the one the
        // platform uses everywhere else rather than an unanswerable request.
        let mut body = json!({
            "model": request.model,
            "max_tokens": request.max_tokens.unwrap_or(4096),
            "messages": messages,
            "stream": stream,
        });
        if !system.is_empty() {
            body["system"] = json!(system.join("\n\n"));
        }
        if let Some(temperature) = request.temperature {
            body["temperature"] = json!(temperature);
        }
        if !request.tools.is_empty() {
            body["tools"] = json!(request
                .tools
                .iter()
                .map(|tool| json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": if tool.parameters.is_null() {
                        json!({ "type": "object", "properties": {} })
                    } else {
                        tool.parameters.clone()
                    },
                }))
                .collect::<Vec<_>>());
        }
        body
    }

    fn parse_answer(&self, value: &Value) -> Option<ChatOutcome> {
        let content = value
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .collect::<String>()
            })
            .unwrap_or_default();
        if content.is_empty() && value.get("content").is_none() {
            return None;
        }

        let finish_reason = value
            .get("stop_reason")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let usage = value.get("usage").and_then(read_anthropic_usage);

        Some(ChatOutcome {
            content,
            finish_reason,
            usage,
            tool_calls: read_anthropic_tool_calls(value.get("content")),
        })
    }

    fn parse_model_list(&self, value: &Value) -> Option<Vec<String>> {
        let entries = value.get("data")?.as_array()?;
        Some(
            entries
                .iter()
                .filter_map(|entry| entry.get("id").and_then(Value::as_str).map(str::to_owned))
                .collect(),
        )
    }

    fn decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(AnthropicStream::default())
    }
}

/// Token counts as the messages protocol reports them.
fn read_anthropic_usage(value: &Value) -> Option<ChatUsage> {
    let input = value.get("input_tokens").and_then(Value::as_u64);
    let output = value.get("output_tokens").and_then(Value::as_u64);
    if input.is_none() && output.is_none() {
        return None;
    }

    Some(ChatUsage {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: input
            .zip(output)
            .map(|(prompt, completion)| prompt + completion),
    })
}

/// Tool calls as the messages protocol reports them: `tool_use` blocks inside `content`.
fn read_anthropic_tool_calls(value: Option<&Value>) -> Vec<crate::client::ChatToolCall> {
    let Some(blocks) = value.and_then(Value::as_array) else {
        return Vec::new();
    };

    blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(|block| {
            Some(crate::client::ChatToolCall {
                id: block.get("id").and_then(Value::as_str)?.to_owned(),
                name: block.get("name").and_then(Value::as_str)?.to_owned(),
                arguments: block
                    .get("input")
                    .filter(|input| input.is_object())
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            })
        })
        .collect()
}

/// Decoder of the messages protocol's typed stream events.
///
/// Usage arrives in two different events — the input count in `message_start`, the output count
/// in `message_delta` — so the decoder holds both and emits the pair when the second arrives.
/// A tool call arrives in three more (`content_block_start` names it, `input_json_delta` carries
/// the arguments a piece at a time, `content_block_stop` closes it), and the block index the
/// provider uses is not the position in the call list, so it is carried through as the piece's
/// index — interleaved text and tool blocks then sort the same way the provider wrote them.
#[derive(Default)]
struct AnthropicStream {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    done: bool,
    block_index: u32,
}

impl StreamDecoder for AnthropicStream {
    fn decode(&mut self, data: &str) -> Result<StreamPiece> {
        let value: Value = serde_json::from_str(data)
            .map_err(|error| AiHubError::Stream(format!("{error}: {}", clip(data))))?;
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| clip(data));
            return Err(AiHubError::Upstream {
                status: 200,
                message,
            });
        }

        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut piece = StreamPiece::default();

        match kind {
            "message_start" => {
                if let Some(usage) = value.pointer("/message/usage") {
                    self.prompt_tokens = usage.get("input_tokens").and_then(Value::as_u64);
                }
            }
            "content_block_start" => {
                self.block_index = value
                    .get("index")
                    .and_then(Value::as_u64)
                    .unwrap_or_default() as u32;
                let block = value.get("content_block").cloned().unwrap_or(Value::Null);
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    piece.tool_call = Some(ToolCallPiece {
                        index: self.block_index,
                        id: block.get("id").and_then(Value::as_str).map(str::to_owned),
                        name: block.get("name").and_then(Value::as_str).map(str::to_owned),
                        arguments_fragment: None,
                    });
                }
            }
            "content_block_delta" => {
                if let Some(text) = value
                    .pointer("/delta/text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    piece.content = Some(text.to_owned());
                }
                if let Some(fragment) = value.pointer("/delta/partial_json").and_then(Value::as_str)
                {
                    piece.tool_call = Some(ToolCallPiece::arguments(self.block_index, fragment));
                }
            }
            "message_delta" => {
                if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    piece.finish_reason = Some(reason.to_owned());
                }
                if let Some(output) = value
                    .pointer("/usage/output_tokens")
                    .and_then(Value::as_u64)
                {
                    self.completion_tokens = Some(output);
                }
                if self.prompt_tokens.is_some() || self.completion_tokens.is_some() {
                    piece.usage = Some(ChatUsage {
                        prompt_tokens: self.prompt_tokens,
                        completion_tokens: self.completion_tokens,
                        total_tokens: self
                            .prompt_tokens
                            .zip(self.completion_tokens)
                            .map(|(prompt, completion)| prompt + completion),
                    });
                }
            }
            "message_stop" => self.done = true,
            // `ping` and `content_block_stop` carry nothing a subscriber sees.
            _ => {}
        }

        Ok(piece)
    }

    fn is_done(&self) -> bool {
        self.done
    }
}

// ---------------------------------------------------------------------------------------------
// Google Gemini
// ---------------------------------------------------------------------------------------------

/// `generateContent`: a `contents` list with `user`/`model` roles and a `usageMetadata` block.
struct GoogleGemini;

impl ProtocolAdapter for GoogleGemini {
    fn protocol(&self) -> &'static str {
        "google_gemini"
    }

    fn capability_note(&self) -> &'static str {
        "generateContent: conversation roles are user/model, token counts live in usageMetadata."
    }

    /// The model is part of the path here, which is why the signature takes it: `generateContent`
    /// is addressed as `/models/{model}:generateContent`, and a streamed answer at
    /// `/models/{model}:streamGenerateContent` — the vendor spells it as a second operation
    /// rather than as a flag in the body.
    fn chat_path(&self, model: &str, stream: bool) -> String {
        let operation = if stream {
            "streamGenerateContent"
        } else {
            "generateContent"
        };
        format!("/models/{model}:{operation}")
    }

    fn models_path(&self) -> String {
        "/models".to_owned()
    }

    fn auth_headers(&self, api_key: Option<&str>) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(key) = api_key.filter(|key| !key.is_empty())
            && let Ok(value) = reqwest::header::HeaderValue::from_str(key)
        {
            headers.insert("x-goog-api-key", value);
        }
        headers
    }

    fn build_chat(&self, request: &ChatRequest, stream: bool) -> Value {
        let system: Vec<&str> = request
            .messages
            .iter()
            .filter(|message| message.role == crate::client::ChatRole::System)
            .map(|message| message.content.as_str())
            .collect();

        // A tool result is a `user` turn holding a `functionResponse` part that names the call
        // it answers, and the model turn that asked for it holds a `functionCall` part. Both
        // are parts rather than roles, so a turn is a list of parts — but they are still
        // *separate turns with different roles*. Merging a `functionResponse` into the same
        // turn as the `functionCall` that asked for it produces a request the provider refuses.
        let mut parts: Vec<Value> = Vec::new();
        let mut part_role = String::new();
        let mut contents: Vec<Value> = Vec::new();
        let mut flush = |parts: &mut Vec<Value>, role: &mut String, contents: &mut Vec<Value>| {
            if !parts.is_empty() {
                contents.push(json!({ "role": role, "parts": parts }));
                parts.clear();
            }
        };
        for message in request
            .messages
            .iter()
            .filter(|message| message.role != crate::client::ChatRole::System)
        {
            use crate::client::ChatRole;
            match message.role {
                ChatRole::Tool => {
                    if part_role != "user" {
                        flush(&mut parts, &mut part_role, &mut contents);
                        part_role = "user".to_owned();
                    }
                    parts.push(json!({
                        "functionResponse": {
                            "name": message.tool_name(),
                            "response": { "result": message.content },
                        }
                    }));
                }
                ChatRole::Assistant if !message.tool_calls.is_empty() => {
                    if part_role != "model" {
                        flush(&mut parts, &mut part_role, &mut contents);
                        part_role = "model".to_owned();
                    }
                    for call in &message.tool_calls {
                        parts.push(json!({
                            "functionCall": { "name": call.name, "args": call.arguments }
                        }));
                    }
                }
                _ => {
                    flush(&mut parts, &mut part_role, &mut contents);
                    contents.push(json!({
                        "role": if message.role == ChatRole::Assistant { "model" } else { "user" },
                        "parts": [{ "text": message.content }],
                    }));
                }
            }
        }
        flush(&mut parts, &mut part_role, &mut contents);

        let mut body = json!({ "contents": contents });
        if !system.is_empty() {
            body["systemInstruction"] = json!({ "parts": [{ "text": system.join("\n\n") }] });
        }
        let mut generation = json!({});
        if let Some(temperature) = request.temperature {
            generation["temperature"] = json!(temperature);
        }
        if let Some(max_tokens) = request.max_tokens {
            generation["maxOutputTokens"] = json!(max_tokens);
        }
        if !generation.as_object().is_none_or(serde_json::Map::is_empty) {
            body["generationConfig"] = generation;
        }
        if !request.tools.is_empty() {
            // Gemini names a tool in the declaration and refers to it by name only — there is
            // no call id anywhere in the protocol, so the platform mints one from the name and
            // the index rather than asking a provider for a handle it does not have.
            body["tools"] = json!([{ "functionDeclarations": request
                .tools
                .iter()
                .map(|tool| json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": if tool.parameters.is_null() {
                        json!({ "type": "object", "properties": {} })
                    } else {
                        tool.parameters.clone()
                    },
                }))
                .collect::<Vec<_>>() }]);
            body["toolConfig"] = json!({ "functionCallingConfig": { "mode": "AUTO" } });
        }
        // The streaming operation is chosen by the path, not by a field in the body, so nothing
        // is added here for it.
        let _ = stream;
        body
    }

    fn parse_answer(&self, value: &Value) -> Option<ChatOutcome> {
        let candidate = value.get("candidates")?.as_array()?.first()?;
        let content: String = candidate
            .pointer("/content/parts")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect()
            })
            .unwrap_or_default();
        let finish_reason = candidate
            .get("finishReason")
            .and_then(Value::as_str)
            .map(str::to_owned);

        Some(ChatOutcome {
            content,
            finish_reason,
            usage: value.get("usageMetadata").and_then(read_gemini_usage),
            tool_calls: read_gemini_tool_calls(candidate.pointer("/content/parts")),
        })
    }

    fn parse_model_list(&self, value: &Value) -> Option<Vec<String>> {
        let entries = value.get("models")?.as_array()?;
        Some(
            entries
                .iter()
                .filter_map(|entry| {
                    // The list carries `models/gemini-1.5-pro`; the chat path expects the bare
                    // name, so the prefix is stripped here rather than at every call site.
                    entry
                        .get("name")
                        .and_then(Value::as_str)
                        .map(|name| name.strip_prefix("models/").unwrap_or(name).to_owned())
                })
                .collect(),
        )
    }

    fn decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(GeminiStream { done: false })
    }
}

/// Token counts as `usageMetadata` reports them.
fn read_gemini_usage(value: &Value) -> Option<ChatUsage> {
    let prompt = value.get("promptTokenCount").and_then(Value::as_u64);
    let completion = value.get("candidatesTokenCount").and_then(Value::as_u64);
    if prompt.is_none() && completion.is_none() {
        return None;
    }

    Some(ChatUsage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: value
            .get("totalTokenCount")
            .and_then(Value::as_u64)
            .or_else(|| prompt.zip(completion).map(|(a, b)| a + b)),
    })
}

/// Tool calls as `generateContent` reports them: `functionCall` parts.
///
/// The protocol has no call id at all, so one is minted from the tool's name. Two calls to the
/// same tool in one turn would then share a handle, which is why the position is part of the id:
/// the answer a call needs is matched by id, and two identical handles would make the model's
/// own result pairing ambiguous.
fn read_gemini_tool_calls(value: Option<&Value>) -> Vec<crate::client::ChatToolCall> {
    let Some(parts) = value.and_then(Value::as_array) else {
        return Vec::new();
    };

    parts
        .iter()
        .filter_map(|part| {
            let call = part.get("functionCall")?;
            let name = call.get("name").and_then(Value::as_str)?;
            let position = parts
                .iter()
                .position(|earlier| earlier.get("functionCall").is_some())
                .unwrap_or(0);
            Some(crate::client::ChatToolCall {
                id: format!("gemini:{name}:{position}"),
                name: name.to_owned(),
                arguments: call
                    .get("args")
                    .filter(|args| args.is_object())
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            })
        })
        .collect()
}

/// Decoder of a `streamGenerateContent` body: the same `candidates` shape, chunk by chunk.
struct GeminiStream {
    done: bool,
}

impl StreamDecoder for GeminiStream {
    fn decode(&mut self, data: &str) -> Result<StreamPiece> {
        let value: Value = serde_json::from_str(data)
            .map_err(|error| AiHubError::Stream(format!("{error}: {}", clip(data))))?;
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| clip(data));
            return Err(AiHubError::Upstream {
                status: 200,
                message,
            });
        }

        let mut piece = StreamPiece::default();
        if let Some(candidate) = value
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|candidates| candidates.first())
        {
            let text: String = candidate
                .pointer("/content/parts")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect()
                })
                .unwrap_or_default();
            if !text.is_empty() {
                piece.content = Some(text);
            }
            if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
                piece.finish_reason = Some(reason.to_owned());
                self.done = true;
            }
            // A chunked stream sends a function call whole in one chunk, so the piece carries
            // the complete arguments as one fragment and the assembler parses it as-is.
            piece.tool_call = candidate
                .pointer("/content/parts")
                .and_then(|parts| parts.as_array().and_then(|parts| parts.first()))
                .and_then(|part| {
                    let call = part.get("functionCall")?;
                    let name = call.get("name").and_then(Value::as_str)?;
                    let arguments = call.get("args").cloned().unwrap_or_else(|| json!({}));
                    Some(ToolCallPiece {
                        index: 0,
                        // No id in this protocol, so the platform mints a stable one from the
                        // name: the same call in the same turn always answers the same handle.
                        id: Some(format!("gemini:{name}")),
                        name: Some(name.to_owned()),
                        arguments_fragment: Some(arguments.to_string()),
                    })
                });
        }
        if let Some(usage) = value.get("usageMetadata").and_then(read_gemini_usage) {
            piece.usage = Some(usage);
        }

        Ok(piece)
    }

    fn is_done(&self) -> bool {
        self.done
    }
}

// ---------------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------------

/// Text out of a content field: a string as-is, an array of parts joined, anything else nothing.
pub fn content_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => Some(
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect(),
        ),
        _ => None,
    }
}

/// A provider message clipped to something a log line can carry.
fn clip(body: &str) -> String {
    crate::client::clip_body(body)
}

/// The role name one protocol uses for a message, as the platform knows it.
#[must_use]
pub fn role_name(protocol: &str, role: &ChatMessage) -> &'static str {
    let assistant = role.role == crate::client::ChatRole::Assistant;
    match protocol {
        "anthropic_messages" => {
            if assistant {
                "assistant"
            } else {
                "user"
            }
        }
        "google_gemini" => {
            if assistant {
                "model"
            } else {
                "user"
            }
        }
        _ => role.role.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{ChatRole, ProviderTarget, ToolSpec};
    use uuid::Uuid;

    fn request(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            model: "mock-model".to_owned(),
            messages,
            temperature: Some(0.3),
            max_tokens: Some(64),
            tools: Vec::new(),
        }
    }

    /// A request that offers one tool, for the declarations under test.
    fn request_with_tool(messages: Vec<ChatMessage>) -> ChatRequest {
        let tool = ToolSpec::new(
            "page.search",
            "Search the pages.",
            serde_json::json!({
                "type": "object",
                "properties": { "q": { "type": "string" } },
                "required": ["q"],
            }),
        );
        ChatRequest::new("mock-model", messages).with_tools(vec![tool])
    }

    fn target() -> ProviderTarget {
        ProviderTarget {
            id: Uuid::nil(),
            name: "Mock".to_owned(),
            protocol: "openai_compatible".to_owned(),
            base_url: "https://api.example.com/v1".to_owned(),
            api_key: Some("sk-test".to_owned()),
            timeout_ms: 30_000,
        }
    }

    #[test]
    fn the_registry_covers_exactly_the_three_adapters() {
        assert_eq!(
            adapter_for("openai_compatible").protocol(),
            "openai_compatible"
        );
        assert_eq!(
            adapter_for("anthropic_messages").protocol(),
            "anthropic_messages"
        );
        assert_eq!(adapter_for("google_gemini").protocol(), "google_gemini");
        assert_eq!(
            protocol_infos()
                .iter()
                .map(|info| info.protocol)
                .collect::<Vec<_>>(),
            crate::model::SUPPORTED_PROTOCOLS,
            "the form's list and the stored list are one list"
        );
    }

    #[test]
    fn each_adapter_owns_its_own_paths_and_auth() {
        let openai = adapter_for("openai_compatible");
        assert_eq!(openai.chat_path("m", true), "/chat/completions");
        assert_eq!(openai.models_path(), "/models");
        let auth = openai.auth_headers(Some("sk-test"));
        assert_eq!(auth.get("authorization").unwrap(), "Bearer sk-test");
        assert!(openai.auth_headers(None).get("authorization").is_none());

        let anthropic = adapter_for("anthropic_messages");
        assert_eq!(anthropic.chat_path("claude", true), "/messages");
        let auth = anthropic.auth_headers(Some("sk-ant"));
        assert_eq!(auth.get("x-api-key").unwrap(), "sk-ant");
        assert!(auth.contains_key("anthropic-version"));
        assert!(!auth.contains_key("authorization"));

        let gemini = adapter_for("google_gemini");
        assert_eq!(
            gemini.chat_path("gemini-1.5-pro", false),
            "/models/gemini-1.5-pro:generateContent"
        );
        assert_eq!(
            gemini.chat_path("gemini-1.5-pro", true),
            "/models/gemini-1.5-pro:streamGenerateContent",
            "the streaming operation is its own path, as the vendor spells it"
        );
        let auth = gemini.auth_headers(Some("AIza"));
        assert_eq!(auth.get("x-goog-api-key").unwrap(), "AIza");
        assert!(!auth.contains_key("authorization"));
    }

    #[test]
    fn the_openai_body_is_unchanged() {
        let body = adapter_for("openai_compatible")
            .build_chat(&request(vec![ChatMessage::user("hi")]), true);
        assert_eq!(body["model"], "mock-model");
        assert_eq!(body["stream"], true);
        assert_eq!(body["temperature"], 0.3);
        assert_eq!(body["max_tokens"], 64);
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn a_stream_asks_for_its_usage_and_a_plain_call_carries_no_stream_options() {
        // OpenAI, vLLM, llama.cpp and Ollama's compat layer all report token counts in a stream
        // **only** when the request asks for them. The field is the ask.
        let streamed = adapter_for("openai_compatible")
            .build_chat(&request(vec![ChatMessage::user("hi")]), true);
        assert_eq!(streamed["stream_options"]["include_usage"], true);

        // A non-streaming answer always carries its usage, so the field would be noise there —
        // and a local runtime that validates its body strictly is the reason to omit it.
        let whole = adapter_for("openai_compatible")
            .build_chat(&request(vec![ChatMessage::user("hi")]), false);
        assert!(
            whole.get("stream_options").is_none(),
            "a non-streaming body carries nothing that only a stream needs"
        );

        // The other two protocols spell it their own way and must stay untouched by this.
        for protocol in ["anthropic_messages", "google_gemini"] {
            let body =
                adapter_for(protocol).build_chat(&request(vec![ChatMessage::user("hi")]), true);
            assert!(
                body.get("stream_options").is_none(),
                "{protocol}: this field is an OpenAI-compatible one"
            );
        }
    }

    #[test]
    fn the_messages_body_lifts_the_system_block_out() {
        let body = adapter_for("anthropic_messages").build_chat(
            &request(vec![
                ChatMessage::system("be brief"),
                ChatMessage::user("hi"),
                ChatMessage {
                    role: ChatRole::Assistant,
                    content: "hello".to_owned(),
                    tool_call_id: None,
                    name: None,
                    tool_calls: Vec::new(),
                },
            ]),
            false,
        );
        assert_eq!(body["system"], "be brief");
        assert_eq!(body["messages"].as_array().expect("turns").len(), 2);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][1]["role"], "assistant");
        assert_eq!(body["max_tokens"], 64, "the protocol requires a budget");
        assert_eq!(body["stream"], false);

        // No answer budget and no system block: the body is still answerable.
        let bare = adapter_for("anthropic_messages").build_chat(
            &ChatRequest {
                model: "claude".to_owned(),
                messages: vec![ChatMessage::user("hi")],
                temperature: None,
                max_tokens: None,
                tools: Vec::new(),
            },
            true,
        );
        assert!(bare.get("system").is_none());
        assert!(bare["max_tokens"].as_u64().expect("a budget") > 0);
    }

    #[test]
    fn the_gemini_body_uses_contents_and_generation_config() {
        let body = adapter_for("google_gemini").build_chat(
            &request(vec![
                ChatMessage::system("be brief"),
                ChatMessage::user("hi"),
                ChatMessage {
                    role: ChatRole::Assistant,
                    content: "hello".to_owned(),
                    tool_call_id: None,
                    name: None,
                    tool_calls: Vec::new(),
                },
            ]),
            false,
        );
        assert_eq!(body["contents"].as_array().expect("turns").len(), 2);
        assert_eq!(body["contents"][0]["role"], "user");
        assert_eq!(body["contents"][1]["role"], "model");
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be brief");
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 64);
        assert_eq!(body["generationConfig"]["temperature"], 0.3);
        assert!(
            body.get("stream").is_none(),
            "the body carries no stream flag: Gemini spells it in the path"
        );
    }

    #[test]
    fn every_adapter_reads_its_own_answer_into_the_same_shape() {
        let openai = adapter_for("openai_compatible")
            .parse_answer(&json!({
                "choices": [{"message": {"content": "Hello"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}
            }))
            .expect("an answer");
        assert_eq!(openai.content, "Hello");
        assert_eq!(openai.finish_reason.as_deref(), Some("stop"));
        assert_eq!(openai.usage.expect("usage").total_tokens, Some(6));

        let anthropic = adapter_for("anthropic_messages")
            .parse_answer(&json!({
                "content": [{"type": "text", "text": "Hello"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 9, "output_tokens": 2}
            }))
            .expect("an answer");
        assert_eq!(anthropic.content, "Hello");
        assert_eq!(anthropic.finish_reason.as_deref(), Some("end_turn"));
        let usage = anthropic.usage.expect("usage");
        assert_eq!(usage.prompt_tokens, Some(9));
        assert_eq!(usage.completion_tokens, Some(2));
        assert_eq!(usage.total_tokens, Some(11), "the two counts are summed");

        let gemini = adapter_for("google_gemini")
            .parse_answer(&json!({
                "candidates": [{"content": {"parts": [{"text": "Hello"}]}, "finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": 4, "candidatesTokenCount": 3, "totalTokenCount": 7}
            }))
            .expect("an answer");
        assert_eq!(gemini.content, "Hello");
        assert_eq!(gemini.finish_reason.as_deref(), Some("STOP"));
        assert_eq!(gemini.usage.expect("usage").total_tokens, Some(7));

        // A body that carries no answer at all is `None` everywhere, not an empty string.
        assert!(
            adapter_for("openai_compatible")
                .parse_answer(&json!({}))
                .is_none()
        );
        assert!(
            adapter_for("anthropic_messages")
                .parse_answer(&json!({}))
                .is_none()
        );
        assert!(
            adapter_for("google_gemini")
                .parse_answer(&json!({}))
                .is_none()
        );
    }

    #[test]
    fn every_adapter_reads_its_own_model_list() {
        let openai = adapter_for("openai_compatible")
            .parse_model_list(&json!({"data": [{"id": "a"}, {"id": "b"}]}))
            .expect("a list");
        assert_eq!(openai, vec!["a".to_owned(), "b".to_owned()]);

        // A bare list is what a few local runtimes answer.
        assert_eq!(
            adapter_for("openai_compatible")
                .parse_model_list(&json!(["local-a", "local-b"]))
                .expect("a list"),
            vec!["local-a".to_owned(), "local-b".to_owned()]
        );

        let anthropic = adapter_for("anthropic_messages")
            .parse_model_list(&json!({"data": [{"id": "claude-x"}]}))
            .expect("a list");
        assert_eq!(anthropic, vec!["claude-x".to_owned()]);

        let gemini = adapter_for("google_gemini")
            .parse_model_list(&json!({"models": [{"name": "models/gemini-1.5-pro"}]}))
            .expect("a list");
        assert_eq!(
            gemini,
            vec!["gemini-1.5-pro".to_owned()],
            "the models/ prefix is stripped once, here"
        );

        assert!(
            adapter_for("openai_compatible")
                .parse_model_list(&json!({}))
                .is_none()
        );
        assert!(
            adapter_for("google_gemini")
                .parse_model_list(&json!({}))
                .is_none()
        );
    }

    #[test]
    fn every_adapter_normalises_its_stream_into_the_same_pieces() {
        // OpenAI: content deltas, a finish reason, then a usage-only frame.
        let mut openai = adapter_for("openai_compatible").decoder();
        let first = openai
            .decode(r#"{"choices":[{"delta":{"content":"Hel"}}]}"#)
            .expect("a piece");
        assert_eq!(first.content.as_deref(), Some("Hel"));
        let last = openai
            .decode(
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":4,"total_tokens":11}}"#,
            )
            .expect("a piece");
        assert_eq!(last.finish_reason.as_deref(), Some("stop"));
        assert_eq!(last.usage.expect("usage").total_tokens, Some(11));
        assert!(!openai.is_done());
        assert!(openai.decode("[DONE]").expect("a piece").is_empty());
        assert!(openai.is_done());

        // Messages: text deltas, a stop reason and the output count in the delta event, the end
        // sentinel in its own event.
        let mut anthropic = adapter_for("anthropic_messages").decoder();
        assert!(
            anthropic
                .decode(r#"{"type":"message_start","message":{"usage":{"input_tokens":7}}}"#)
                .expect("a piece")
                .is_empty()
        );
        let text = anthropic
            .decode(r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hel"}}"#)
            .expect("a piece");
        assert_eq!(text.content.as_deref(), Some("Hel"));
        let stop = anthropic
            .decode(r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}"#)
            .expect("a piece");
        assert_eq!(stop.finish_reason.as_deref(), Some("end_turn"));
        let usage = stop.usage.expect("both counts");
        assert_eq!(
            usage.prompt_tokens,
            Some(7),
            "the input count survives from message_start"
        );
        assert_eq!(usage.completion_tokens, Some(4));
        assert_eq!(usage.total_tokens, Some(11));
        assert!(
            anthropic
                .decode(r#"{"type":"message_stop"}"#)
                .expect("a piece")
                .is_empty()
        );
        assert!(anthropic.is_done());

        // Gemini: the same candidates shape, and the finish reason is also the end sentinel.
        let mut gemini = adapter_for("google_gemini").decoder();
        let chunk = gemini
            .decode(r#"{"candidates":[{"content":{"parts":[{"text":"Hel"}]}}]}"#)
            .expect("a piece");
        assert_eq!(chunk.content.as_deref(), Some("Hel"));
        let end = gemini
            .decode(r#"{"candidates":[{"content":{"parts":[]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":2,"totalTokenCount":3}}"#)
            .expect("a piece");
        assert_eq!(end.finish_reason.as_deref(), Some("STOP"));
        assert_eq!(end.usage.expect("usage").total_tokens, Some(3));
        assert!(gemini.is_done());
    }

    #[test]
    fn a_vendor_error_inside_a_stream_is_one_error_not_a_piece() {
        for protocol in ["openai_compatible", "anthropic_messages", "google_gemini"] {
            let payload = match protocol {
                "openai_compatible" => r#"{"error":{"message":"no such model"}}"#,
                "anthropic_messages" => r#"{"type":"error","error":{"message":"overloaded"}}"#,
                _ => r#"{"error":{"code":429,"message":"quota"}}"#,
            };
            let mut decoder = adapter_for(protocol).decoder();
            let error = decoder.decode(payload).expect_err("a provider error");
            assert!(
                matches!(error, AiHubError::Upstream { status: 200, .. }),
                "{protocol}"
            );
            let text = error.to_string();
            assert!(
                ["no such model", "overloaded", "quota"]
                    .iter()
                    .any(|needle| text.contains(needle)),
                "{protocol}: the provider's own words survive: {text}"
            );
        }
    }

    #[test]
    fn a_frame_that_is_not_json_is_a_stream_failure() {
        let mut decoder = adapter_for("openai_compatible").decoder();
        let error = decoder.decode("{not json").expect_err("a stream failure");
        assert!(matches!(error, AiHubError::Stream(_)));
    }

    #[test]
    fn roles_are_named_the_way_each_protocol_names_them() {
        let user = ChatMessage::user("hi");
        let assistant = ChatMessage {
            role: ChatRole::Assistant,
            content: "hello".to_owned(),
                    tool_call_id: None,
                    name: None,
                    tool_calls: Vec::new(),
                };
        assert_eq!(role_name("openai_compatible", &user), "user");
        assert_eq!(role_name("openai_compatible", &assistant), "assistant");
        assert_eq!(role_name("anthropic_messages", &assistant), "assistant");
        assert_eq!(role_name("google_gemini", &assistant), "model");
        assert_eq!(role_name("google_gemini", &user), "user");
    }

    #[test]
    fn the_target_still_reaches_its_adapter() {
        let mut target = target();
        target.protocol = "google_gemini".to_owned();
        assert_eq!(adapter_for(&target.protocol).protocol(), "google_gemini");
        assert_eq!(
            target.endpoint(&adapter_for(&target.protocol).chat_path("gemini-x", false)),
            "https://api.example.com/v1/models/gemini-x:generateContent"
        );
    }

    // -------------------------------------------------------------------------------------
    // Tool declarations: all three protocols
    // -------------------------------------------------------------------------------------

    #[test]
    fn no_tools_means_no_declarations_in_any_protocol() {
        // An empty `tools` array is not the same as no field: several providers answer one
        // with a 400, so the adapters have to omit the key entirely.
        for protocol in ["openai_compatible", "anthropic_messages", "google_gemini"] {
            let body = adapter_for(protocol)
                .build_chat(&request(vec![ChatMessage::user("hi")]), false);
            assert!(
                body.get("tools").is_none() && body.get("toolConfig").is_none(),
                "{protocol} sent a tools field for a request with no tools"
            );
        }
    }

    #[test]
    fn openai_declares_functions_and_asks_for_one() {
        let body = adapter_for("openai_compatible")
            .build_chat(&request_with_tool(vec![ChatMessage::user("find it")]), false);

        let tools = body["tools"].as_array().expect("a tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "page.search");
        assert_eq!(tools[0]["function"]["parameters"]["required"][0], "q");
        // Without the flag a tool-capable model is free to answer in prose and never call.
        assert_eq!(body["tool_choice"], "auto");
    }

    #[test]
    fn anthropic_declares_input_schemas() {
        let body = adapter_for("anthropic_messages")
            .build_chat(&request_with_tool(vec![ChatMessage::user("find it")]), false);

        let tool = &body["tools"][0];
        assert_eq!(tool["name"], "page.search");
        // The messages protocol spells a tool's schema `input_schema`, not `parameters`.
        assert_eq!(tool["input_schema"]["properties"]["q"]["type"], "string");
    }

    #[test]
    fn gemini_wraps_declarations_and_sets_the_calling_mode() {
        let body = adapter_for("google_gemini")
            .build_chat(&request_with_tool(vec![ChatMessage::user("find it")]), false);

        let declarations = body["tools"][0]["functionDeclarations"]
            .as_array()
            .expect("declarations");
        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations[0]["name"], "page.search");
        assert_eq!(body["toolConfig"]["functionCallingConfig"]["mode"], "AUTO");
    }

    // -------------------------------------------------------------------------------------
    // Tool results travel back with the pairing that makes them legal
    // -------------------------------------------------------------------------------------

    fn conversation_after_a_call(id: &str) -> Vec<ChatMessage> {
        vec![
            ChatMessage::user("what is the weather in Istanbul?"),
            ChatMessage {
                role: ChatRole::Assistant,
                content: String::new(),
                tool_call_id: None,
                name: None,
                tool_calls: vec![crate::client::ChatToolCall {
                    id: id.to_owned(),
                    name: "weather.lookup".to_owned(),
                    arguments: serde_json::json!({ "city": "Istanbul" }),
                }],
            },
            ChatMessage::tool_result(id, "weather.lookup", "18C and clear"),
        ]
    }

    #[test]
    fn openai_pairs_a_result_with_the_assistant_turn_that_asked_for_it() {
        let body = adapter_for("openai_compatible")
            .build_chat(&ChatRequest::new("mock-model", conversation_after_a_call("call_1")), false);

        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 3);
        // The assistant turn must carry the call, or the result below is unmatched.
        assert_eq!(messages[1]["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            messages[1]["tool_calls"][0]["function"]["arguments"],
            "{\"city\":\"Istanbul\"}"
        );
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "call_1");
    }

    #[test]
    fn anthropic_pairs_a_result_as_a_tool_result_block() {
        let body = adapter_for("anthropic_messages")
            .build_chat(&ChatRequest::new("mock-model", conversation_after_a_call("toolu_1")), false);

        let messages = body["messages"].as_array().expect("messages");
        // The messages protocol has no `tool` role: the call rides on an assistant turn as a
        // `tool_use` block and the result on a user turn as a `tool_result` block.
        assert_eq!(messages[1]["content"][0]["type"], "tool_use");
        assert_eq!(messages[1]["content"][0]["id"], "toolu_1");
        assert_eq!(messages[1]["content"][0]["input"]["city"], "Istanbul");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu_1");
    }

    #[test]
    fn gemini_pairs_a_result_as_a_function_response() {
        let body = adapter_for("google_gemini")
            .build_chat(&ChatRequest::new("mock-model", conversation_after_a_call("g1")), false);

        let contents = body["contents"].as_array().expect("contents");
        let call = &contents[1]["parts"][0]["functionCall"];
        assert_eq!(call["name"], "weather.lookup");
        assert_eq!(call["args"]["city"], "Istanbul");
        let response = &contents[2]["parts"][0]["functionResponse"];
        assert_eq!(response["name"], "weather.lookup");
        assert_eq!(response["response"]["result"], "18C and clear");
    }

    // -------------------------------------------------------------------------------------
    // Reading a provider's tool calls back
    // -------------------------------------------------------------------------------------

    #[test]
    fn openai_reads_tool_calls_out_of_a_non_streamed_answer() {
        let value = serde_json::json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_9",
                        "type": "function",
                        "function": { "name": "page.search", "arguments": "{\"q\":\"x\"}" },
                    }],
                },
                "finish_reason": "tool_calls",
            }],
            "usage": { "prompt_tokens": 11, "completion_tokens": 4 },
        });
        let answer = adapter_for("openai_compatible").parse_answer(&value).expect("an answer");

        assert_eq!(answer.tool_calls.len(), 1);
        assert_eq!(answer.tool_calls[0].id, "call_9");
        assert_eq!(answer.tool_calls[0].name, "page.search");
        assert_eq!(answer.tool_calls[0].arguments["q"], "x");
        // The reason string says "tool_calls" and the turn is *not* final.
        assert!(!answer.is_final());
    }

    #[test]
    fn a_malformed_argument_blob_becomes_an_empty_object_rather_than_failing_the_turn() {
        // A model that emits half a JSON object is ordinary. The tool's own validator then
        // refuses it with a message the model can read and correct.
        let value = serde_json::json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "c",
                        "function": { "name": "page.search", "arguments": "{\"q\":" },
                    }],
                },
                "finish_reason": "tool_calls",
            }]
        });
        let answer = adapter_for("openai_compatible").parse_answer(&value).expect("an answer");

        assert_eq!(answer.tool_calls[0].arguments, serde_json::json!({}));
    }

    #[test]
    fn a_turn_with_tool_calls_and_no_text_is_still_a_turn() {
        // The regression this guards: "no text" was treated as "nothing arrived", which ended
        // a run on step one, before the first tool had run.
        let value = serde_json::json!({
            "choices": [{
                "message": { "tool_calls": [{
                    "id": "c",
                    "function": { "name": "page.search", "arguments": "{}" },
                }]},
                "finish_reason": "tool_calls",
            }]
        });
        let answer = adapter_for("openai_compatible").parse_answer(&value).expect("an answer");

        assert!(answer.content.is_empty());
        assert_eq!(answer.tool_calls.len(), 1);
    }

    #[test]
    fn anthropic_reads_tool_use_blocks() {
        let value = serde_json::json!({
            "content": [
                { "type": "text", "text": "one moment" },
                { "type": "tool_use", "id": "toolu_7", "name": "page.search", "input": { "q": "x" } },
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 8, "output_tokens": 3 },
        });
        let answer = adapter_for("anthropic_messages")
            .parse_answer(&value)
            .expect("an answer");

        assert_eq!(answer.content, "one moment");
        assert_eq!(answer.tool_calls.len(), 1);
        assert_eq!(answer.tool_calls[0].id, "toolu_7");
        assert!(!answer.is_final());
    }

    #[test]
    fn gemini_reads_function_call_parts() {
        let value = serde_json::json!({
            "candidates": [{
                "content": { "parts": [
                    { "functionCall": { "name": "page.search", "args": { "q": "x" } } }
                ]},
                "finishReason": "STOP",
            }],
            "usageMetadata": { "promptTokenCount": 5, "candidatesTokenCount": 2 },
        });
        let answer = adapter_for("google_gemini").parse_answer(&value).expect("an answer");

        assert_eq!(answer.tool_calls.len(), 1);
        // Gemini issues no call id, so the platform mints one — and it has to be unique per
        // call, because a result is matched to its call by id.
        assert_eq!(answer.tool_calls[0].id, "gemini:page.search:0");
        assert!(!answer.is_final());
    }

    // -------------------------------------------------------------------------------------
    // Streamed tool calls
    // -------------------------------------------------------------------------------------

    #[test]
    fn a_streamed_tool_call_arrives_in_pieces_an_index_and_all() {
        let mut decoder = adapter_for("openai_compatible").decoder();

        let first = decoder
            .decode(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"page.search","arguments":""}}]}}]}"#)
            .expect("decodes");
        let piece = first.tool_call.expect("a tool-call piece");
        assert_eq!(piece.index, 0);
        assert_eq!(piece.id.as_deref(), Some("call_1"));
        assert_eq!(piece.name.as_deref(), Some("page.search"));

        // The argument JSON is split mid-token, which is the ordinary case.
        let second = decoder
            .decode(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"q\":"}}]}}]}"#)
            .expect("decodes");
        assert_eq!(
            second.tool_call.expect("a piece").arguments_fragment.as_deref(),
            Some("{\"q\":")
        );

        let third = decoder
            .decode(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"x\"}"}}]}}]}"#)
            .expect("decodes");
        assert_eq!(
            third.tool_call.expect("a piece").arguments_fragment.as_deref(),
            Some("\"x\"}")
        );

        // Text still decodes alongside it.
        let text = decoder
            .decode(r#"{"choices":[{"delta":{"content":"hi"}}]}"#)
            .expect("decodes");
        assert_eq!(text.content.as_deref(), Some("hi"));
    }

    #[test]
    fn an_interleaved_pair_of_calls_keeps_its_own_arguments() {
        let mut assembler = crate::client::ToolCallAssembler::default();
        let mut decoder = adapter_for("openai_compatible").decoder();
        for frame in [
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"one","arguments":"{\"x\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"two","arguments":"{\"y\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"2}"}}]}}]}"#,
        ] {
            if let Some(piece) = decoder.decode(frame).expect("decodes").tool_call {
                assert!(assembler.push(piece), "the assembler must accept a fragment");
            }
        }
        let calls = assembler.finish(|position| position as u32);

        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "one");
        assert_eq!(calls[0].arguments["x"], 1);
        assert_eq!(calls[1].name, "two");
        assert_eq!(calls[1].arguments["y"], 2);
    }

    #[test]
    fn anthropic_carries_the_block_index_into_the_argument_fragments() {
        let mut decoder = adapter_for("anthropic_messages").decoder();
        let start = decoder
            .decode(r#"{"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"toolu_3","name":"page.search"}}"#)
            .expect("decodes");
        let piece = start.tool_call.expect("a piece");
        assert_eq!(piece.index, 3);
        assert_eq!(piece.id.as_deref(), Some("toolu_3"));

        // The fragment has to land on the same index, or it lands on another call.
        let delta = decoder
            .decode(r#"{"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"q\":"}}"#)
            .expect("decodes");
        assert_eq!(delta.tool_call.expect("a piece").index, 3);
    }

    #[test]
    fn the_assembler_refuses_a_call_whose_arguments_never_stop_growing() {
        let mut assembler = crate::client::ToolCallAssembler::default();
        let chunk = "x".repeat(1024 * 1024);
        let mut accepted = 0;
        for _ in 0..8 {
            let piece = ToolCallPiece {
                index: 0,
                id: Some("c".to_owned()),
                name: Some("t".to_owned()),
                arguments_fragment: Some(chunk.clone()),
            };
            if !assembler.push(piece) {
                break;
            }
            accepted += 1;
        }
        assert!(accepted < 8, "an unbounded argument buffer must be refused");
    }
}
