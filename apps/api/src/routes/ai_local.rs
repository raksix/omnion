//! `/api/v1/ai/local/*` — local endpoints and their models (REQ-106, slice 1).
//!
//! | Endpoint | Power | What it does |
//! |---|---|---|
//! | `GET /ai/local/endpoints` | `ai.local.read` | Every provider with its verified locality, host kind and model counts |
//! | `POST /ai/local/endpoints` | `ai.local.manage` | Register a local endpoint; the host is verified, not trusted |
//! | `GET /ai/local/models` | `ai.local.read` | What the endpoints serve, filtered |
//! | `POST /ai/local/models/pull` | `ai.local.manage` | Claim and start a pull; a second click is a no-op with a reason |
//! | `POST /ai/local/models/cancel` | `ai.local.manage` | Cancel a pull in flight |
//! | `POST /ai/local/models/retry` | `ai.local.manage` | Clear an `error` row and pull again |
//! | `DELETE /ai/local/models` | `ai.local.manage` | Remove a model, refusing while it is pulling |
//! | `POST /ai/local/scan` | `ai.local.manage` | Ask an endpoint what it serves and record the answer |
//!
//! # The HTTP to the local server lives here, not in the store
//!
//! `local_store` is database work and is proven against a database with no network in the picture;
//! everything that dials an endpoint is in this file, which is also where the QA stub server can
//! stand in for a real Ollama. That split is not tidiness: it means a slow local server can never
//! hold a database connection while it thinks, and it means the "local endpoint refuses a redirect"
//! acceptance criterion has exactly one implementation to break.
//!
//! # Every endpoint call goes through the redirect-refusing client
//!
//! `local_client()` is [`omnion_ai_hub::local_host::local_http`], never the shared pooled client.
//! A local URL that answers `302` is the one event that must not be followed: the check passed
//! about the URL it was given, and a followed redirect puts the bytes somewhere no locality check
//! has classified. The refusal names the destination.
//!
//! # Locality in the response body is the *stored* value
//!
//! The list answers with `locality` as the database holds it, not as the request re-derives it.
//! A screen that re-derived the answer could disagree with the air-gap check, and the whole point
//! of the column is that there is one answer (see the store's module docs).

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use omnion_ai_hub::error::{AiHubError, Result as HubResult};
use omnion_ai_hub::local_host::{host_of, local_http, refuse_redirect};
use omnion_ai_hub::local_store::{
    self, LocalEndpoint, LocalModel, ModelFilter, NewLocalEndpoint, PullOutcome, ServedModel,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

/// Longest progress/error line kept from a local server, and shown verbatim.
///
/// A server's own words are the useful part of a failed pull ("no such model", "insufficient
/// disk"), and an unbounded field would put whatever the endpoint chose into a jsonb column and a
/// table cell. The clip happens once, here.
const MAX_SERVER_CHARS: usize = 500;

// -------------------------------------------------------------------------------------------
// Endpoints
// -------------------------------------------------------------------------------------------

/// `POST /ai/local/endpoints` body.
#[derive(Debug, Deserialize)]
pub struct CreateEndpointBody {
    /// Display name.
    pub name: String,
    /// Absolute base URL, version segment included.
    pub base_url: String,
    /// Wire protocol. Local servers documented by this request all speak `openai_compatible`.
    #[serde(default)]
    pub protocol: Option<String>,
    /// A key, when the local server wants one.
    #[serde(default)]
    pub api_key: Option<String>,
    /// `true` registers it as a local endpoint. The host is verified either way — this value
    /// cannot make a public address local, and omitting it stores the endpoint as remote.
    #[serde(default = "default_true")]
    pub local: bool,
}

fn default_true() -> bool {
    true
}

/// `GET /ai/local/endpoints`.
#[derive(Debug, Serialize)]
pub struct EndpointsResponse {
    /// Every provider, local ones first.
    pub endpoints: Vec<LocalEndpoint>,
    /// How many are verified local — the stat tile's number.
    pub local_count: usize,
    /// How many are verified remote, i.e. whose traffic can leave this machine.
    pub remote_count: usize,
    /// `true` when no endpoint has ever been registered, so the panel shows its empty state
    /// rather than an empty table it has to guess the meaning of.
    pub is_empty: bool,
}

/// `GET /ai/local/endpoints` — the endpoints with their verified locality.
pub async fn list_endpoints(
    State(state): State<AppState>,
) -> Result<Json<EndpointsResponse>, ApiError> {
    let pool = state.db().pool();
    let endpoints = local_store::list_endpoints(pool).await?;
    let local_count = endpoints.iter().filter(|e| e.locality == "local").count();
    let remote_count = endpoints.len().saturating_sub(local_count);
    Ok(Json(EndpointsResponse {
        is_empty: endpoints.is_empty(),
        endpoints,
        local_count,
        remote_count,
    }))
}

/// `POST /ai/local/endpoints` — register a local endpoint.
///
/// # Errors
///
/// `invalid_provider` with the host named, when the address is not local or is a metadata
/// endpoint. The validation is in the store, not here, so the air-gap check and the doctor
/// classify with the same function.
pub async fn create_endpoint(
    State(state): State<AppState>,
    Json(body): Json<CreateEndpointBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let pool = state.db().pool();
    let name = body.name.trim().to_owned();
    if name.is_empty() {
        return Err(ApiError::bad_request(
            "invalid_provider",
            "an endpoint needs a name — the model table groups by it",
        ));
    }
    let base_url = omnion_ai_hub::model::normalize_base_url(&body.base_url)
        .map_err(|error| ApiError::bad_request("invalid_provider", error.to_string()))?;
    let protocol = body
        .protocol
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("openai_compatible")
        .to_owned();
    omnion_ai_hub::model::validate_protocol(&protocol)
        .map_err(|error| ApiError::bad_request("invalid_provider", error.to_string()))?;

    let id = local_store::create_endpoint(
        pool,
        NewLocalEndpoint {
            name,
            base_url,
            protocol,
            requested_local: body.local,
            api_key: body.api_key,
        },
    )
    .await?;

    let endpoint = local_store::get_endpoint(pool, id).await?;
    // A newly registered endpoint has never answered, so `last_seen_at` is NULL and the panel
    // shows "not checked yet" rather than a green badge nobody measured.
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "endpoint": endpoint })),
    ))
}

// -------------------------------------------------------------------------------------------
// Models
// -------------------------------------------------------------------------------------------

/// `GET /ai/local/models` query.
#[derive(Debug, Default, Deserialize)]
pub struct ModelQuery {
    /// Restrict to one endpoint (uuid).
    pub endpoint: Option<String>,
    /// Restrict to one status.
    pub status: Option<String>,
    /// `tools`, `vision`, `embeddings` or `rerank`.
    pub capability: Option<String>,
    /// Free text over the key and the display name.
    pub q: Option<String>,
}

/// `GET /ai/local/models` — what the endpoints serve.
#[derive(Debug, Serialize)]
pub struct ModelsResponse {
    /// The rows after filtering.
    pub models: Vec<LocalModel>,
    /// The totals the panel's stat tiles read: models, how many are available, how many are
    /// resident, and how many are mid-pull. `is_empty` is computed on the *unfiltered* list, so
    /// a search that matches nothing says "no match" and an installation with no local models at
    /// all says "nothing registered" — two different empty states, which the request asks for.
    pub total_models: usize,
    pub available: usize,
    pub resident: usize,
    pub pulling: usize,
    pub is_empty: bool,
}

/// `GET /ai/local/models` — the model table.
pub async fn list_models(
    State(state): State<AppState>,
    Query(query): Query<ModelQuery>,
) -> Result<Json<ModelsResponse>, ApiError> {
    let pool = state.db().pool();
    if let Some(status) = query.status.as_deref() {
        if !omnion_ai_hub::local_store::MODEL_STATUSES.contains(&status) {
            return Err(ApiError::bad_request(
                "invalid_model",
                format!(
                    "`{status}` is not a model status (one of: {})",
                    omnion_ai_hub::local_store::MODEL_STATUSES.join(", ")
                ),
            ));
        }
    }
    let provider_id = match query
        .endpoint
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(raw) => Some(Uuid::parse_str(raw).map_err(|_| {
            ApiError::bad_request("invalid_model", format!("`{raw}` is not an endpoint id"))
        })?),
        None => None,
    };
    let filter = ModelFilter {
        provider_id,
        status: query.status.clone(),
        capability: query.capability.clone(),
        search: query.q.clone(),
    };
    let all = local_store::list_models(pool, &ModelFilter::default()).await?;
    let models = local_store::list_models(pool, &filter).await?;
    Ok(Json(ModelsResponse {
        available: all.iter().filter(|m| m.status == "available").count(),
        resident: all.iter().filter(|m| m.resident).count(),
        pulling: all.iter().filter(|m| m.status == "pulling").count(),
        total_models: all.len(),
        is_empty: all.is_empty(),
        models,
    }))
}

/// A pull request: which endpoint, which model key.
#[derive(Debug, Deserialize)]
pub struct ModelKeyBody {
    /// Endpoint id.
    pub endpoint: Uuid,
    /// The model's key as the endpoint spells it (`llama3.1:8b`).
    pub model_key: String,
}

/// `POST /ai/local/models/pull`.
///
/// Claims the pull in the database **first**, then calls the server. The order matters: a claim
/// followed by a failed call leaves an `error` row the operator can see and retry, while a call
/// followed by a claim would let two concurrent requests both start a download and then discover
/// they lost the race — with two writes and one download nobody is tracking.
pub async fn pull_model(
    State(state): State<AppState>,
    Json(body): Json<ModelKeyBody>,
) -> Result<Json<Value>, ApiError> {
    let pool = state.db().pool();
    let key = body.model_key.trim().to_owned();
    let (outcome, row) = local_store::begin_pull(pool, body.endpoint, &key).await?;
    if outcome != PullOutcome::Started {
        // Not an error: the request is idempotent and the panel needs to *say* why the drawer did
        // not open. `409` would be the wrong status — nothing is in conflict, the model is simply
        // already there or already coming.
        return Ok(Json(serde_json::json!({
            "outcome": outcome.as_str(),
            "model": row,
        })));
    }

    let endpoint = local_store::get_endpoint(pool, body.endpoint).await?;
    match pull_from_server(&endpoint, &key).await {
        Ok(message) => {
            local_store::finish_pull(pool, body.endpoint, &key, Some(&message)).await?;
            let model = local_store::get_model(pool, body.endpoint, &key).await?;
            Ok(Json(
                serde_json::json!({ "outcome": "available", "model": model }),
            ))
        }
        Err(error) => {
            // The server's own words, clipped, and the row kept in `error` so the table shows the
            // failure instead of the model quietly disappearing.
            let message = clip(&error.to_string());
            local_store::mark_pull_failed(pool, body.endpoint, &key, &message).await?;
            let model = local_store::get_model(pool, body.endpoint, &key).await?;
            Err(
                ApiError::new(StatusCode::BAD_GATEWAY, "local_pull_failed", message)
                    .with_details(serde_json::json!({ "model": model })),
            )
        }
    }
}

/// `POST /ai/local/models/cancel` — stop a pull in flight.
pub async fn cancel_pull(
    State(state): State<AppState>,
    Json(body): Json<ModelKeyBody>,
) -> Result<Json<Value>, ApiError> {
    let pool = state.db().pool();
    local_store::cancel_pull(pool, body.endpoint, &body.model_key).await?;
    let model = local_store::get_model(pool, body.endpoint, &body.model_key).await?;
    Ok(Json(serde_json::json!({ "model": model })))
}

/// `POST /ai/local/models/retry` — clear an `error` row and pull it again.
pub async fn retry_pull(
    State(state): State<AppState>,
    Json(body): Json<ModelKeyBody>,
) -> Result<Json<Value>, ApiError> {
    let pool = state.db().pool();
    let key = body.model_key.trim().to_owned();
    let outcome = local_store::reset_for_pull(pool, body.endpoint, &key).await?;
    if outcome != PullOutcome::Started {
        let row = local_store::get_model(pool, body.endpoint, &key).await?;
        return Ok(Json(
            serde_json::json!({ "outcome": outcome.as_str(), "model": row }),
        ));
    }
    let endpoint = local_store::get_endpoint(pool, body.endpoint).await?;
    match pull_from_server(&endpoint, &key).await {
        Ok(message) => {
            local_store::finish_pull(pool, body.endpoint, &key, Some(&message)).await?;
            let model = local_store::get_model(pool, body.endpoint, &key).await?;
            Ok(Json(
                serde_json::json!({ "outcome": "available", "model": model }),
            ))
        }
        Err(error) => {
            let message = clip(&error.to_string());
            local_store::mark_pull_failed(pool, body.endpoint, &key, &message).await?;
            Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                "local_pull_failed",
                message,
            ))
        }
    }
}

/// `DELETE /ai/local/models` — remove a model from its endpoint.
pub async fn remove_model(
    State(state): State<AppState>,
    Json(body): Json<ModelKeyBody>,
) -> Result<Json<Value>, ApiError> {
    let pool = state.db().pool();
    let endpoint = local_store::get_endpoint(pool, body.endpoint).await?;
    local_store::remove_model(pool, body.endpoint, &body.model_key).await?;
    Ok(Json(serde_json::json!({
        "removed": body.model_key.trim(),
        "endpoint": endpoint.name,
    })))
}

/// `POST /ai/local/scan` — ask an endpoint what it serves and record the answer.
///
/// # Errors
///
/// `502 local_endpoint_unreachable` with the endpoint's own message when the server does not
/// answer, and `422` when it answers something this platform cannot read. The two are different
/// faults — "your Ollama is down" and "your Ollama speaks something else" — and collapsing them
/// into one "connection failed" is what leaves an operator re-checking a server that was up.
pub async fn scan_endpoint(
    State(state): State<AppState>,
    Json(body): Json<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let pool = state.db().pool();
    let endpoint = local_store::get_endpoint(pool, body).await?;
    let models = fetch_served_models(&endpoint).await?;
    let written = local_store::upsert_served_models(pool, body, &models).await?;
    local_store::touch_endpoint(pool, body).await?;
    Ok(Json(serde_json::json!({
        "endpoint": endpoint.name,
        "served": models.len(),
        "written": written,
    })))
}

// -------------------------------------------------------------------------------------------
// The local server client
// -------------------------------------------------------------------------------------------

/// Ask a local endpoint to pull a model, and answer with the server's own line on success.
///
/// Ollama's `POST /api/pull` answers a stream of JSON lines and holds the connection until the
/// download finishes; the last line carries the status. `llama.cpp`'s server has no pull at all
/// and 404s, which is reported as the server's own refusal rather than a platform sentence.
async fn pull_from_server(endpoint: &LocalEndpoint, model_key: &str) -> HubResult<String> {
    let url = format!("{}/api/pull", endpoint.base_url.trim_end_matches('/'));
    let response = local_client()
        .post(&url)
        .json(&serde_json::json!({ "model": model_key, "stream": true }))
        .send()
        .await
        .map_err(|error| {
            AiHubError::Transport(format!(
                "could not reach the local endpoint at {}: {error}",
                endpoint.base_url
            ))
        })?;

    if response.status().is_redirection() {
        return Err(refuse_redirect(&url, &location_of(&response)));
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(AiHubError::Upstream {
            status: status.as_u16(),
            message: clip(&body),
        });
    }
    // The stream's last line is the verdict; earlier lines are progress.
    Ok(last_stream_line(&body))
}

/// Read what an endpoint serves, from the OpenAI-compatible model list.
///
/// **One shape, with the two spellings normalized.** llama.cpp and LM Studio answer
/// `{"data":[{"id":…}]}`; Ollama answers `{"models":[{"model":…}]}`. Both are read here rather
/// than being made the caller's problem, because a screen that shows an empty table for a server
/// that is serving three models is indistinguishable from an empty installation.
async fn fetch_served_models(endpoint: &LocalEndpoint) -> HubResult<Vec<ServedModel>> {
    let url = format!("{}/models", endpoint.base_url.trim_end_matches('/'));
    let response = local_client().get(&url).send().await.map_err(|error| {
        AiHubError::Transport(format!(
            "could not reach the local endpoint at {}: {error}",
            endpoint.base_url
        ))
    })?;

    if response.status().is_redirection() {
        return Err(refuse_redirect(&url, &location_of(&response)));
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(AiHubError::Upstream {
            status: status.as_u16(),
            message: clip(&body),
        });
    }
    parse_model_list(&body)
}

/// Turn a local server's model list into rows.
///
/// `pub` in spirit and in signature so the walk can prove it against a recorded body: the
/// capability flags and the key spelling are where a local-server integration goes wrong, and a
/// parser that is only reachable through a live socket cannot be tested without one.
fn parse_model_list(body: &str) -> HubResult<Vec<ServedModel>> {
    let value: Value = serde_json::from_str(body)
        .map_err(|error| AiHubError::Malformed(format!("the model list is not JSON: {error}")))?;
    let entries = value
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| value.get("models").and_then(Value::as_array))
        .ok_or_else(|| {
            AiHubError::Malformed(
                "the model list has neither `data` (OpenAI-compatible) nor `models` (Ollama)"
                    .to_owned(),
            )
        })?;

    let mut models = Vec::with_capacity(entries.len());
    for entry in entries {
        // The key is `id` on an OpenAI-compatible server and `model` on Ollama; `name` is llama.cpp's
        // own spelling in some builds. Missing all three is skipped rather than invented, because
        // a row with an empty key cannot be pulled, removed or matched to anything.
        let key = ["id", "model", "name"]
            .into_iter()
            .find_map(|field| entry.get(field).and_then(Value::as_str))
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let Some(key) = key else { continue };

        let details = entry.get("details");
        let embedding_dimension = entry
            .get("embedding_length")
            .or_else(|| entry.get("embedding_dimension"))
            .and_then(Value::as_i64)
            .and_then(|value| i32::try_from(value).ok());
        let capabilities = entry.get("capabilities").and_then(Value::as_array);
        let supports = |name: &str| {
            capabilities.is_some_and(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .any(|value| value.eq_ignore_ascii_case(name))
            })
        };

        models.push(ServedModel {
            model_key: key.to_owned(),
            display_name: entry
                .get("display_name")
                .or_else(|| entry.get("name_detail"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            size_bytes: entry.get("size").and_then(Value::as_i64),
            parameter_count: details
                .and_then(|d| d.get("parameter_size"))
                .and_then(Value::as_str)
                .and_then(parse_parameter_count),
            quantization: details
                .and_then(|d| d.get("quantization_level"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            context_window: entry
                .get("context_length")
                .and_then(Value::as_i64)
                .and_then(|value| i32::try_from(value).ok()),
            supports_tools: supports("tools"),
            supports_vision: supports("vision"),
            // Ollama does not advertise a capabilities array at all on older builds; an
            // embedding model there would be recorded as "no" rather than guessed, and the doctor
            // is the screen that tells an operator their embedding model is not advertised.
            supports_embeddings: supports("embedding") || supports("embeddings"),
            supports_rerank: supports("rerank"),
            embedding_dimension,
        });
    }
    Ok(models)
}

/// Ollama's `details.parameter_size` is a display string like `"8.0B"`, so it is parsed rather
/// than stored verbatim in a `bigint` the screen would have to format differently.
fn parse_parameter_count(raw: &str) -> Option<i64> {
    let digits: String = raw
        .trim()
        .trim_end_matches(['B', 'b', 'M', 'm'])
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let value: f64 = digits.parse().ok()?;
    let multiplier = if raw.trim_end().ends_with(['M', 'm']) {
        1_000_000
    } else {
        1_000_000_000
    };
    Some((value * multiplier as f64) as i64)
}

/// The `Location` header of a `3xx`, or an honest "no location" string.
///
/// A refusal that says "redirected, destination unknown" is still a refusal — the control fired —
/// but the request asks for the *target*, so the header is read and its absence is stated rather
/// than papered over with the request URL again.
fn location_of(response: &reqwest::Response) -> String {
    response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_else(|| "(the answer carried no Location header)".to_owned())
}

/// The last JSON line of a pull stream, or the whole body when it was not a stream.
fn last_stream_line(body: &str) -> String {
    body.lines()
        .rev()
        .find_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            let value: Value = serde_json::from_str(trimmed).ok()?;
            let status = value.get("status").and_then(Value::as_str)?;
            let detail = value.get("error").and_then(Value::as_str);
            Some(match detail {
                Some(error) => format!("{status}: {error}"),
                None => status.to_owned(),
            })
        })
        .unwrap_or_else(|| clip(body))
}

/// Clip a server's words to [`MAX_SERVER_CHARS`] with an honest marker.
fn clip(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX_SERVER_CHARS {
        return trimmed.to_owned();
    }
    let head: String = trimmed.chars().take(MAX_SERVER_CHARS).collect();
    format!("{head}… (truncated)")
}

/// The redirect-refusing client. Named so the choice is visible at every call site.
fn local_client() -> reqwest::Client {
    local_http()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_model_list_spellings_parse() {
        let openai = r#"{"data":[{"id":"qwen2.5-coder:7b","object":"model"},
            {"id":"nomic-embed-text","object":"model"}]}"#;
        let keys: Vec<String> = parse_model_list(openai)
            .expect("an OpenAI-compatible list")
            .into_iter()
            .map(|m| m.model_key)
            .collect();
        assert_eq!(keys, vec!["qwen2.5-coder:7b", "nomic-embed-text"]);

        let ollama = r#"{"models":[{"model":"llama3.1:8b","size":4661224676,
            "details":{"parameter_size":"8.0B","quantization_level":"Q4_K_M"}}]}"#;
        let models = parse_model_list(ollama).expect("an Ollama list");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].size_bytes, Some(4_661_224_676));
        assert_eq!(models[0].parameter_count, Some(8_000_000_000));
        assert_eq!(models[0].quantization.as_deref(), Some("Q4_K_M"));
    }

    #[test]
    fn capability_flags_come_from_the_servers_own_array() {
        let body = r#"{"data":[
            {"id":"embed-model","capabilities":["embedding"]},
            {"id":"vision-model","capabilities":["completion","vision","tools"]},
            {"id":"reranker","capabilities":["rerank"]}]}"#;
        let models = parse_model_list(body).expect("a list");
        assert!(models[0].supports_embeddings, "embedding");
        assert!(!models[0].supports_tools);
        assert!(models[1].supports_vision && models[1].supports_tools);
        assert!(models[2].supports_rerank);
    }

    #[test]
    fn an_entry_with_no_key_is_skipped_rather_than_invented() {
        // A row with an empty key cannot be pulled or removed, so it must not exist.
        let body = r#"{"data":[{"object":"model"},{"id":"  "},{"id":"real-model"}]}"#;
        let keys: Vec<String> = parse_model_list(body)
            .expect("a list")
            .into_iter()
            .map(|m| m.model_key)
            .collect();
        assert_eq!(keys, vec!["real-model"]);
    }

    #[test]
    fn a_body_with_neither_key_is_a_readable_failure() {
        let error = parse_model_list(r#"{"object":"list"}"#).expect_err("neither key");
        let text = error.to_string();
        assert!(text.contains("data"), "{text}");
        assert!(text.contains("models"), "{text}");
    }

    #[test]
    fn the_pull_stream_reports_its_last_line() {
        let stream = concat!(
            r#"{"status":"pulling manifest"}"#,
            "\n",
            r#"{"status":"downloading 45%"}"#,
            "\n",
            r#"{"status":"success"}"#,
            "\n"
        );
        assert_eq!(last_stream_line(stream), "success");
        // An error line names both halves, because the status alone ("error") says nothing.
        let failed = "{\"status\":\"error\",\"error\":\"insufficient disk\"}\n";
        assert_eq!(last_stream_line(failed), "error: insufficient disk");
        // A non-stream body is still shown, clipped.
        assert_eq!(last_stream_line("plain text answer"), "plain text answer");
    }

    #[test]
    fn parameter_counts_scale_with_their_suffix() {
        assert_eq!(parse_parameter_count("8.0B"), Some(8_000_000_000));
        assert_eq!(parse_parameter_count("137M"), Some(137_000_000));
        assert_eq!(parse_parameter_count("nonsense"), None);
    }

    #[test]
    fn clipping_marks_what_it_dropped() {
        let short = clip("  a short answer  ");
        assert_eq!(short, "a short answer");
        let long = "x".repeat(MAX_SERVER_CHARS + 50);
        let clipped = clip(&long);
        assert!(clipped.ends_with("(truncated)"));
        assert_eq!(
            clipped.chars().filter(|c| *c == 'x').count(),
            MAX_SERVER_CHARS
        );
    }
}
