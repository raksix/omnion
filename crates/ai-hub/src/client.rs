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

use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::error::{AiHubError, Result};
use crate::model::Provider;

/// Time allowed to open the connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Time allowed for one non-streaming answer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
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
}

impl ChatRole {
    /// Wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }

    /// Read a wire name.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "system" => Ok(Self::System),
            "user" => Ok(Self::User),
            "assistant" => Ok(Self::Assistant),
            other => Err(AiHubError::InvalidChatRequest(format!(
                "unknown message role \"{other}\" (system, user, assistant)"
            ))),
        }
    }
}

/// One message of a chat request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Who said it.
    pub role: ChatRole,
    /// What was said.
    pub content: String,
}

impl ChatMessage {
    /// A user message.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::User,
            content: content.into(),
        }
    }

    /// A system message.
    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::System,
            content: content.into(),
        }
    }
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
}

/// Token counts a provider reported.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatUsage {
    /// Input tokens.
    pub prompt_tokens: Option<u64>,
    /// Output tokens.
    pub completion_tokens: Option<u64>,
    /// Total tokens (providers disagree whether it includes both; reported as-is).
    pub total_tokens: Option<u64>,
}

/// A finished answer.
#[derive(Debug, Clone)]
pub struct ChatOutcome {
    /// The answer text.
    pub content: String,
    /// Why the model stopped, as the provider put it.
    pub finish_reason: Option<String>,
    /// Token counts, when the provider reported them.
    pub usage: Option<ChatUsage>,
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
        .any(|message| message.content.trim().is_empty())
    {
        return Err(AiHubError::InvalidChatRequest(
            "chat messages may not be empty".to_owned(),
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

/// The JSON body of one chat call.
fn body(request: &ChatRequest, stream: bool) -> Value {
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

/// A conversation, answered in one piece.
pub async fn chat(target: &ProviderTarget, request: &ChatRequest) -> Result<ChatOutcome> {
    validate_request(request)?;

    let response = http()
        .post(target.endpoint("/chat/completions"))
        .headers(auth_headers(target))
        .json(&body(request, false))
        .timeout(REQUEST_TIMEOUT)
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

    let outcome = read_completion(&value).ok_or_else(|| {
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

    let mut response = http()
        .post(target.endpoint("/chat/completions"))
        .headers(auth_headers(target))
        .json(&body(request, true))
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

    let mut parser = StreamParser::default();
    let mut outcome = ChatOutcome {
        content: String::new(),
        finish_reason: None,
        usage: None,
    };

    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| AiHubError::Stream(error.to_string()))?
    {
        for piece in parser.push(&String::from_utf8_lossy(&chunk)) {
            let piece = piece?;
            if let Some(text) = piece.content {
                if outcome.content.len() + text.len() > MAX_ANSWER_BYTES {
                    return Err(AiHubError::Stream(
                        "the provider sent more than the platform accepts for one answer"
                            .to_owned(),
                    ));
                }
                outcome.content.push_str(&text);
                if events.send(ChatEvent::Delta(text)).await.is_err() {
                    // The caller stopped listening: the answer in hand is still the answer.
                    return Ok(outcome);
                }
            }
            if piece.finish_reason.is_some() {
                outcome.finish_reason = piece.finish_reason;
            }
            if piece.usage.is_some() {
                outcome.usage = piece.usage;
            }
        }
    }

    for piece in parser.finish() {
        let piece = piece?;
        if let Some(text) = piece.content {
            outcome.content.push_str(&text);
            let _ = events.send(ChatEvent::Delta(text)).await;
        }
        if piece.finish_reason.is_some() {
            outcome.finish_reason = piece.finish_reason;
        }
        if piece.usage.is_some() {
            outcome.usage = piece.usage;
        }
    }

    if outcome.content.is_empty() && outcome.finish_reason.is_none() {
        return Err(AiHubError::Malformed(
            "the provider streamed no answer and no finish reason".to_owned(),
        ));
    }

    Ok(outcome)
}

/// The provider's own model list.
pub async fn list_remote_models(target: &ProviderTarget) -> Result<Vec<String>> {
    let response = http()
        .get(target.endpoint("/models"))
        .headers(auth_headers(target))
        .timeout(REQUEST_TIMEOUT)
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

    // OpenAI answers `{"data":[{"id": "…"}]}`; a few local runtimes answer a bare list.
    let entries = value
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| value.as_array())
        .ok_or_else(|| {
            AiHubError::Malformed(format!("no model list in the response: {}", trimmed(&text)))
        })?;

    let mut models: Vec<String> = entries
        .iter()
        .filter_map(|entry| {
            entry
                .get("id")
                .and_then(Value::as_str)
                .or_else(|| entry.as_str())
                .map(str::to_owned)
        })
        .filter(|id| !id.trim().is_empty())
        .collect();
    models.sort();
    models.dedup();

    Ok(models)
}

/// Authentication and identification headers of one provider call.
fn auth_headers(target: &ProviderTarget) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(key) = &target.api_key {
        if let Ok(value) = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}")) {
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
    }
    headers
}

/// A refusal reported inside an otherwise successful answer.
fn refused(value: &Value) -> Result<()> {
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

/// The answer of a non-streaming completion.
fn read_completion(value: &Value) -> Option<ChatOutcome> {
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

/// Text out of a content field: a string as-is, an array of parts joined, anything else empty.
fn content_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let text: String = parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect();
            Some(text)
        }
        _ => None,
    }
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

/// One decoded piece of a provider's answer stream.
#[derive(Debug, Default)]
struct StreamPiece {
    content: Option<String>,
    finish_reason: Option<String>,
    usage: Option<ChatUsage>,
}

/// Decoder for the `text/event-stream` frames of an OpenAI-compatible answer.
///
/// It is deliberately forgiving: providers wrap frames differently (`\n\n` vs `\r\n\r\n`), send
/// keep-alive comments, omit the trailing blank line, and disagree about whether usage arrives
/// in a final frame. The decoder holds the incomplete tail of a frame until the rest of it
/// arrives — a chunk boundary in the middle of a JSON object must never lose an answer.
#[derive(Debug, Default)]
struct StreamParser {
    buffer: String,
    done: bool,
}

impl StreamParser {
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
        if data.trim() == "[DONE]" {
            self.done = true;
            return None;
        }

        Some(self.read_piece(&data))
    }

    /// Read one JSON frame.
    fn read_piece(&self, data: &str) -> Result<StreamPiece> {
        let value: Value = serde_json::from_str(data)
            .map_err(|error| AiHubError::Stream(format!("{error}: {}", trimmed(data))))?;
        refused(&value)?;

        let mut piece = StreamPiece::default();
        if let Some(choice) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        {
            if let Some(content) = choice.pointer("/delta/content").and_then(content_text) {
                if !content.is_empty() {
                    piece.content = Some(content);
                }
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> ProviderTarget {
        ProviderTarget {
            id: Uuid::nil(),
            name: "Mock".to_owned(),
            protocol: "openai_compatible".to_owned(),
            base_url: "https://api.example.com/v1/".to_owned(),
            api_key: Some("sk-test".to_owned()),
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
            base_url: "http://127.0.0.1:11434/v1/".to_owned(),
            api_key: Some(String::new()),
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
    fn the_body_carries_only_what_the_caller_set() {
        let minimal = body(
            &ChatRequest {
                model: "mock-small".to_owned(),
                messages: vec![ChatMessage::user("hi")],
                temperature: None,
                max_tokens: None,
            },
            true,
        );
        assert_eq!(minimal["stream"], json!(true));
        assert!(minimal.get("temperature").is_none());
        assert!(minimal.get("max_tokens").is_none());
        assert_eq!(minimal["messages"][0]["role"], json!("user"));

        let full = body(
            &ChatRequest {
                model: "mock-large".to_owned(),
                messages: vec![ChatMessage::system("be brief"), ChatMessage::user("hi")],
                temperature: Some(0.2),
                max_tokens: Some(64),
            },
            false,
        );
        assert_eq!(full["temperature"], json!(0.2));
        assert_eq!(full["max_tokens"], json!(64));
        assert_eq!(full["stream"], json!(false));
    }

    #[test]
    fn the_parser_reads_openai_frames() {
        let mut parser = StreamParser::default();
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
        let mut parser = StreamParser::default();
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
        let mut parser = StreamParser::default();
        let pieces = parser.push(
            ": keep-alive\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\r\n\r\ndata: [DONE]\r\n\r\n",
        );
        assert_eq!(pieces.len(), 1);
        assert!(parser.done);

        let mut trailing = StreamParser::default();
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
        let mut parser = StreamParser::default();
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
        let mut parser = StreamParser::default();
        let pieces = parser.push("data: {\"error\":{\"message\":\"model is offline\"}}\n\n");
        let error = pieces[0].as_ref().expect_err("refusal");
        assert!(matches!(error, AiHubError::Upstream { status: 200, .. }));
        assert!(error.to_string().contains("model is offline"));
    }

    #[test]
    fn a_non_streaming_answer_is_read() {
        let value = json!({
            "choices": [{"message": {"role": "assistant", "content": "Hello"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}
        });
        let outcome = read_completion(&value).expect("an answer");
        assert_eq!(outcome.content, "Hello");
        assert_eq!(outcome.finish_reason.as_deref(), Some("stop"));
        assert_eq!(outcome.usage.expect("usage").total_tokens, Some(6));
    }

    #[test]
    fn content_parts_are_joined() {
        let value = json!({"choices": [{"message": {"content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]}}]});
        assert_eq!(read_completion(&value).expect("answer").content, "ab");
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
