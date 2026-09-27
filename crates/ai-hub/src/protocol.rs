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
#[derive(Debug, Default, Clone, PartialEq)]
pub struct StreamPiece {
    /// A piece of the answer, when this frame carried one.
    pub content: Option<String>,
    /// Why the model stopped, when the provider said so in this frame.
    pub finish_reason: Option<String>,
    /// Token counts, when the provider reported them in this frame.
    pub usage: Option<ChatUsage>,
}

impl StreamPiece {
    /// `true` when the piece carries nothing a subscriber can see.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.content.is_none() && self.finish_reason.is_none() && self.usage.is_none()
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
            "messages": request.messages,
            "stream": stream,
        });
        if let Some(temperature) = request.temperature {
            body["temperature"] = json!(temperature);
        }
        if let Some(max_tokens) = request.max_tokens {
            body["max_tokens"] = json!(max_tokens);
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
        headers.insert("content-type", reqwest::header::HeaderValue::from_static("application/json"));
        headers
    }

    fn build_chat(&self, request: &ChatRequest, stream: bool) -> Value {
        let system: Vec<&str> = request
            .messages
            .iter()
            .filter(|message| message.role == crate::client::ChatRole::System)
            .map(|message| message.content.as_str())
            .collect();
        let messages: Vec<Value> = request
            .messages
            .iter()
            .filter(|message| message.role != crate::client::ChatRole::System)
            .map(|message| {
                json!({
                    // The messages protocol has no `system` role: it carries instructions
                    // outside the conversation, and only user/assistant turn inside it.
                    "role": if message.role == crate::client::ChatRole::Assistant {
                        "assistant"
                    } else {
                        "user"
                    },
                    "content": message.content,
                })
            })
            .collect();

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
        })
    }

    fn parse_model_list(&self, value: &Value) -> Option<Vec<String>> {
        let entries = value.get("data")?.as_array()?;
        Some(
            entries
                .iter()
                .filter_map(|entry| {
                    entry
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
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
        total_tokens: input.zip(output).map(|(prompt, completion)| prompt + completion),
    })
}

/// Decoder of the messages protocol's typed stream events.
///
/// Usage arrives in two different events — the input count in `message_start`, the output count
/// in `message_delta` — so the decoder holds both and emits the pair when the second arrives.
#[derive(Default)]
struct AnthropicStream {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    done: bool,
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
            return Err(AiHubError::Upstream { status: 200, message });
        }

        let kind = value.get("type").and_then(Value::as_str).unwrap_or_default();
        let mut piece = StreamPiece::default();

        match kind {
            "message_start" => {
                if let Some(usage) = value.pointer("/message/usage") {
                    self.prompt_tokens = usage.get("input_tokens").and_then(Value::as_u64);
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
            }
            "message_delta" => {
                if let Some(reason) = value
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                {
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
            // `ping` and `content_block_start` carry nothing a subscriber sees.
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
        let contents: Vec<Value> = request
            .messages
            .iter()
            .filter(|message| message.role != crate::client::ChatRole::System)
            .map(|message| {
                json!({
                    "role": if message.role == crate::client::ChatRole::Assistant {
                        "model"
                    } else {
                        "user"
                    },
                    "parts": [{ "text": message.content }],
                })
            })
            .collect();

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
        // The streaming operation is chosen by the path, not by a field in the body, so nothing
        // is added here for it.
        let _ = stream;
        body
    }

    fn parse_answer(&self, value: &Value) -> Option<ChatOutcome> {
        let candidate = value
            .get("candidates")?
            .as_array()?
            .first()?;
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
            return Err(AiHubError::Upstream { status: 200, message });
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
            if assistant { "assistant" } else { "user" }
        }
        "google_gemini" => {
            if assistant { "model" } else { "user" }
        }
        _ => role.role.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{ChatRole, ProviderTarget};
    use uuid::Uuid;

    fn request(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            model: "mock-model".to_owned(),
            messages,
            temperature: Some(0.3),
            max_tokens: Some(64),
        }
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
        assert_eq!(adapter_for("openai_compatible").protocol(), "openai_compatible");
        assert_eq!(adapter_for("anthropic_messages").protocol(), "anthropic_messages");
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
    fn the_messages_body_lifts_the_system_block_out() {
        let body = adapter_for("anthropic_messages").build_chat(
            &request(vec![
                ChatMessage::system("be brief"),
                ChatMessage::user("hi"),
                ChatMessage {
                    role: ChatRole::Assistant,
                    content: "hello".to_owned(),
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
        assert!(adapter_for("openai_compatible").parse_answer(&json!({})).is_none());
        assert!(adapter_for("anthropic_messages").parse_answer(&json!({})).is_none());
        assert!(adapter_for("google_gemini").parse_answer(&json!({})).is_none());
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

        assert!(adapter_for("openai_compatible").parse_model_list(&json!({})).is_none());
        assert!(adapter_for("google_gemini").parse_model_list(&json!({})).is_none());
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
        assert!(anthropic
            .decode(r#"{"type":"message_start","message":{"usage":{"input_tokens":7}}}"#)
            .expect("a piece")
            .is_empty());
        let text = anthropic
            .decode(r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hel"}}"#)
            .expect("a piece");
        assert_eq!(text.content.as_deref(), Some("Hel"));
        let stop = anthropic
            .decode(r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}"#)
            .expect("a piece");
        assert_eq!(stop.finish_reason.as_deref(), Some("end_turn"));
        let usage = stop.usage.expect("both counts");
        assert_eq!(usage.prompt_tokens, Some(7), "the input count survives from message_start");
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
            assert!(matches!(error, AiHubError::Upstream { status: 200, .. }), "{protocol}");
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
}
