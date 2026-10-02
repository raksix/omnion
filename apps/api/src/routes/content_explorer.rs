//! `/api/v1/content-api/explorer` — the Explorer tab's dispatcher (REQ-019, slice 3).
//!
//! The Explorer makes **real calls** against the headless surface with a token the caller picks.
//! That sounds like a proxy and is deliberately not one, because of one fact about this product:
//!
//! **A token's plaintext is shown once and stored as a digest.** `api_tokens::hash_secret` means
//! there is no value anywhere — not in the database, not in the panel's session, not in this
//! process — that could be replayed as `Authorization: Bearer …` on a second request. A proxy
//! that accepted a plaintext would be asking the operator to paste a credential that is
//! unrecoverable and unrotatable into a text box, which is the exact shape of a leak this
//! surface has spent three slices refusing to create.
//!
//! So the Explorer **dispatches**: it loads the token row, builds the [`ContentToken`] the read
//! surface's extractor would have built, **spends the same budget from the same meter**, and then
//! sends the request through [`crate::routes::router`] — the real router, with the resolved URL —
//! so the matched route, the path decoding, the `Query` extractor and the handler are all the
//! ones an integrator's own call goes through. The slice's done line ("`x-ratelimit-remaining`
//! decreasing") is therefore measured on the limiter itself rather than on a copy of it.
//!
//! Five rules this route holds, each one a way the obvious implementation goes wrong:
//!
//! 1. **The caller names an `operationId`, never a URL.** The path template comes from the
//!    server's own [`ENDPOINTS`] table, so a request cannot aim the platform at a host, a scheme
//!    or a path of the caller's choosing. A `path` field here would be a server-side request
//!    forgery with somebody's token attached to it.
//! 2. **Parameter names are checked against the document**, so a typo is a `400` naming the
//!    field rather than a silently dropped query string — and a dropped parameter is the worst
//!    kind of explorer bug, because the response still looks right.
//! 3. **A path parameter is percent-encoded into its segment**, so a slug containing `/`, a `?`
//!    or a newline cannot leave the segment it belongs to. It is then decoded by the real
//!    router's `Path` extractor, which is the only decoder the route will ever see.
//! 4. **The metered route is the template**, not the resolved path, so one logical endpoint is
//!    one row on the Usage tab's leaderboard however many slugs were walked. Recording the
//!    resolved path would be the honest thing about the log and the useless thing about the chart.
//! 5. **The snippets carry `$OMNION_TOKEN`, never a credential** — see [`snippets_of`].

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use omnion_content::api_tokens::{self, AuthenticatedToken};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Instant;
use tower::ServiceExt;
use uuid::Uuid;

use crate::auth::CurrentSession;
use crate::content_meter;
use crate::error::ApiError;
use crate::routes::content_api::{organization_of, token_status};
use crate::routes::content_openapi::{ENDPOINTS, Endpoint};
use crate::routes::content_read::{self, ContentCall, ContentToken};
use crate::state::AppState;

/// Longest parameter value accepted, in bytes.
///
/// A slug is short. A caller who pastes a 4 KB "slug" is either debugging something else or
/// probing, and both are answered by the same `400` rather than by a query the database has to
/// chew through.
const MAX_PARAM_VALUE: usize = 512;

/// How many parameters one call may carry. The largest documented endpoint takes ten.
const MAX_PARAMS: usize = 32;

/// Largest response body read back for the pane.
///
/// `limit` is capped at 100 rows by the read surface, so a legitimate response is far smaller;
/// the cap exists so a malformed route cannot stream an unbounded body into a tab.
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// RFC 3986 unreserved set: what may stay literal inside a path segment or a query value.
///
/// `NON_ALPHANUMERIC` encodes `-`, `_`, `.` and `~` too, and those four are exactly the
/// characters RFC 3986 §2.3 says a URL may carry unescaped. Everything else — `/`, `?`, `#`,
/// `&`, `=`, a space, a newline — is encoded, which is what keeps a parameter inside the segment
/// it was written for. The set is a `const` item rather than a literal in the function because
/// `percent_encode` takes it by value, and building it per call would rebuild the table per
/// request for no reason.
const UNRESERVED: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

// ---------------------------------------------------------------------------------------------
// The request
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/content-api/explorer` — make one documented call as one chosen token.
#[derive(Debug, Deserialize)]
pub struct ExplorerCall {
    /// The token to act as, by id.
    ///
    /// By **id**, because by anything else the caller would have to hand over the secret, and the
    /// secret is not a thing this installation can produce twice.
    pub token_id: Uuid,
    /// The OpenAPI `operationId` to call, e.g. `pages.list`.
    pub operation_id: String,
    /// Query and path parameter values by name.
    #[serde(default)]
    pub params: BTreeMap<String, String>,
}

// ---------------------------------------------------------------------------------------------
// The answer
// ---------------------------------------------------------------------------------------------

/// One response header worth showing.
///
/// A curated list rather than every header: the ones a caller branches on are the contract, and
/// an integrator reading a response pane wants `etag` and `x-ratelimit-remaining`, not the
/// `content-length` of a body they are already looking at.
#[derive(Debug, Serialize)]
pub struct HeaderPair {
    /// Header name, as sent.
    pub name: String,
    /// Header value.
    pub value: String,
}

/// The three renderings of the very same resolved request.
#[derive(Debug, Serialize)]
pub struct Snippets {
    /// `curl` for a shell.
    pub curl: String,
    /// `fetch` for a browser.
    pub fetch: String,
    /// `requests` for Python.
    pub python: String,
}

/// Which token acted, without any part of its secret.
#[derive(Debug, Serialize)]
pub struct TokenEcho {
    /// Row id.
    pub id: Uuid,
    /// Display name.
    pub name: String,
    /// The `omn_xxxxxxxx` marker.
    pub prefix: String,
    /// Requests-per-minute tier, so a reader can compare the refusals with the budget.
    pub rate_limit_per_minute: i32,
    /// Budget left in this minute after the call, or `null` when nothing was counted.
    pub remaining: Option<i32>,
}

/// What the call produced, and everything the panel needs to show it honestly.
#[derive(Debug, Serialize)]
pub struct ExplorerAnswer {
    /// The operation that was called.
    pub operation_id: String,
    /// Its HTTP method, from the document.
    pub method: String,
    /// The **resolved** request URL, base included, exactly as it was dispatched.
    pub url: String,
    /// The surface route this call was metered against (`/content/pages`).
    ///
    /// Carried on the wire because the Usage tab's leaderboard keys on it, and a person
    /// comparing the Explorer against the chart needs to know which row the call landed in.
    pub metered_route: String,
    /// Status the read surface answered.
    pub status: u16,
    /// The headers a caller branches on.
    pub headers: Vec<HeaderPair>,
    /// The response body: parsed JSON, or the raw text as a JSON string when it is not JSON.
    pub body: Value,
    /// Whether `body` is the parsed document rather than a string.
    pub body_is_json: bool,
    /// Wall-clock milliseconds the dispatch took, measured around the router call only.
    pub duration_ms: u64,
    /// `next_cursor` lifted out of a list response, or `null`.
    ///
    /// Lifted so the screen's "use cursor for next page" is one assignment rather than a JSON
    /// path walk into a body it is also rendering — two reads of one response that can disagree
    /// about what the response said.
    pub next_cursor: Option<String>,
    /// The token that made the call.
    pub token: TokenEcho,
    /// The call as a caller would write it themselves.
    pub snippets: Snippets,
}

// ---------------------------------------------------------------------------------------------
// The handler
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/content-api/explorer` — execute one documented call as one token.
///
/// **`HeaderMap` before `Json`, deliberately.** Axum splits its extractors in two: everything
/// `FromRequestParts` runs first and `FromRequest` runs last, because only one of them may consume
/// the body. `HeaderMap` is a parts extractor and `Json` is not, so a signature carrying the JSON
/// first is not a style question — it does not satisfy `Handler` and the route will not register.
pub async fn explorer_call(
    State(state): State<AppState>,
    current: CurrentSession,
    headers: axum::http::HeaderMap,
    axum::extract::Json(body): axum::extract::Json<ExplorerCall>,
) -> Result<axum::response::Response, ApiError> {
    let organization_id = organization_of(&current)?;
    let token_row = api_tokens::get_token(state.db().pool(), organization_id, body.token_id)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                format!("reading the token: {error}"),
            )
        })?
        // A token of another organization is *absent*, not forbidden: the caller's session has no
        // business learning that an id exists somewhere else on the installation.
        .ok_or_else(|| {
            ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such content API token")
        })?;

    // A revoked or expired token cannot authenticate, so calling as it would prove nothing —
    // and the screen would be teaching the reader that a dead credential works. The codes are
    // the surface's own, which is what makes the Explorer's error rendering the same shape a real
    // caller's is.
    match token_status(token_row.revoked_at, token_row.expires_at) {
        "revoked" => {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "token_revoked",
                "this token was revoked; the Explorer cannot call as it",
            ));
        }
        "expired" => {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "token_expired",
                "this token has expired; rotate it before calling as it again",
            ));
        }
        _ => {}
    }

    let endpoint = endpoint_of(&body.operation_id)?;
    // `Resolved` carries the acting token's identity because the answer has to name the token it
    // used, and threading it as a second return value would mean two things describing one call.
    let authenticated = AuthenticatedToken { token: token_row };
    let resolved = resolve(endpoint, &body.params, &authenticated)?;
    let origin = origin_of(&headers);

    // The budget, spent exactly as [`ContentToken`]'s extractor spends it: same meter, same
    // round trip, same tier read from the row. The Explorer being a panel route does not make it
    // exempt — an explorer that could burn a customer's quota without touching their limiter is
    // a denial of service with a user interface.
    let verdict = content_meter::spend(
        &state,
        authenticated.token.id,
        authenticated.token.rate_limit_per_minute,
        &resolved.surface,
    )
    .await;
    if verdict.limited {
        return Err(content_read::rate_limited(&verdict, &resolved.surface));
    }

    // The extractor's own side effect, repeated here for the same reason it lives there: a call
    // made through the Explorer is a real use of the token, so the Tokens tab's "last used"
    // column must not be able to disagree with the Usage tab about whether it happened.
    let _ =
        sqlx::query("update api_tokens set last_used_at = now(), updated_at = now() where id = $1")
            .bind(authenticated.token.id)
            .execute(state.db().pool())
            .await;

    let request = dispatch_request(
        endpoint,
        &resolved,
        ContentToken(authenticated),
        ContentCall {
            route: resolved.surface.clone(),
            rate: verdict,
        },
    )?;

    let started = Instant::now();
    let response = crate::routes::router(state.clone())
        .oneshot(request)
        .await
        // The router is infallible for a request it accepted; a join error would be a bug in
        // this route rather than in the caller's call, and it is answered as such.
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                format!("the call could not be dispatched: {error}"),
            )
        })?;
    let elapsed = started.elapsed();

    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), MAX_RESPONSE_BYTES)
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "the surface's response could not be read",
            )
        })?;
    let (body, body_is_json) = match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) => (value, true),
        // A body that is not JSON is shown as text rather than refused: the surface only ever
        // answers JSON today, and a future non-JSON route should appear in the pane as itself
        // instead of becoming an `internal_error` with no explanation.
        Err(_) => (
            Value::String(String::from_utf8_lossy(&bytes).into_owned()),
            false,
        ),
    };
    let next_cursor = body
        .get("next_cursor")
        .and_then(Value::as_str)
        .map(str::to_owned);

    // **One `/api/v1`, and it comes from the document.** `ENDPOINTS[].path` already carries the
    // mount point (`/api/v1/content/pages`), because that is what the OpenAPI document must say for
    // a generated client. Composing the caller's URL by prefixing the mount point again produced
    // `https://host/api/v1/api/v1/content/pages` — a snippet an integrator pastes into a shell and
    // gets a `404` from, and a resolved-URL line that disagrees with the request that actually
    // went out. The origin and the path are two halves of one URL and nothing else.
    let url = format!("{origin}{}", resolved.url());
    // Built here rather than in the literal: the snippets and the `url` field are the same string,
    // and a `clone()` inside the literal would be a second copy of one URL that could drift.
    let snippets = snippets_of(&endpoint.method.to_uppercase(), &url);
    let answer = ExplorerAnswer {
        operation_id: endpoint.id.to_owned(),
        method: endpoint.method.to_uppercase(),
        url,
        metered_route: resolved.surface.clone(),
        status: status.as_u16(),
        headers: interesting_headers(&headers),
        body,
        body_is_json,
        duration_ms: elapsed.as_millis().try_into().unwrap_or(u64::MAX),
        next_cursor,
        token: TokenEcho {
            id: resolved.token_id,
            name: resolved.token_name.clone(),
            prefix: resolved.token_prefix.clone(),
            rate_limit_per_minute: resolved.rate_limit_per_minute,
            remaining: verdict.remaining(),
        },
        snippets,
    };
    Ok(Json(answer).into_response())
}

/// The operation in the document that exists **only to be read**.
///
/// `/api/v1/media` carries `media.panelCrud`, and it is not a content API operation: it is the
/// panel's own session-authenticated media CRUD surface, added to the document as a warning so an
/// integrator reads why they must not call it. Naming it as a constant rather than filtering by
/// string keeps the refusal and the document's reason in one place — a call that tried to
/// dispatch it would reach the panel's surface with a content token, which is the security trap
/// the entry exists to prevent.
pub const DOCUMENTED_BUT_NOT_DISPATCHABLE: &str = "media.panelCrud";

/// The documented operation named, or a `400` that lists the ones that exist.
///
/// A `404` would be the wrong answer twice over: the operation does exist, and a caller who typos
/// `pages.list` as `page.list` needs the list, not a guess.
///
/// `media.panelCrud` is refused with the reason attached rather than answered: it is in the
/// document, so "not one of the documented operations" would be false and send a reader looking
/// for an endpoint that is not there.
pub fn endpoint_of(operation_id: &str) -> Result<&'static Endpoint, ApiError> {
    if operation_id == DOCUMENTED_BUT_NOT_DISPATCHABLE {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "not_dispatchable",
            "/api/v1/media is the panel's own media surface, authenticated with a session; \
             the headless media endpoint is /api/v1/content/media",
        )
        .with_details(json!({ "field": "operation_id", "use": "media.list" })));
    }
    ENDPOINTS
        .iter()
        .find(|endpoint| endpoint.id == operation_id)
        .ok_or_else(|| {
            let known: Vec<&str> = ENDPOINTS.iter().map(|endpoint| endpoint.id).collect();
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_parameter",
                format!(
                    "operation_id must be one of the {} documented operations",
                    known.len()
                ),
            )
            .with_details(json!({
                "field": "operation_id",
                "received": operation_id,
                "accepted": known,
            }))
        })
}

/// The origin a caller's snippets are written against.
///
/// From the request's own authority, for the reason the OpenAPI routes use: a hard-coded host
/// makes every snippet wrong on every installation but one.
fn origin_of(headers: &axum::http::HeaderMap) -> String {
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(axum::http::header::HOST))
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost");
    format!("https://{host}")
}

/// Build the request that goes through the router, with the pre-authenticated token in it.
///
/// The two extensions are the whole trick: [`ContentToken`] and [`ContentCall`] are extractors,
/// and an extractor that finds its value already in the extensions returns it rather than
/// authenticating again. That is why the Explorer does not need to bypass the extractor, disable
/// the limiter or re-implement scope checking — it puts the answer in the place the extractor
/// would have put it, and everything downstream is the real thing.
fn dispatch_request(
    endpoint: &Endpoint,
    resolved: &Resolved,
    token: ContentToken,
    call: ContentCall,
) -> Result<Request<Body>, ApiError> {
    let method = axum::http::Method::from_bytes(endpoint.method.to_uppercase().as_bytes())
        .map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "the documented method is not an HTTP method",
            )
        })?;
    let mut builder = Request::builder().method(method).uri(resolved.url());
    // An `Authorization` header is still sent, deliberately, and is the surface's own decision
    // what to do with it: the extractor returns the pre-authenticated token without reading it,
    // so nothing depends on this value, and a route added later that *does* read it will see a
    // well-formed credential rather than a missing one.
    builder = builder.header(
        axum::http::header::AUTHORIZATION,
        "Bearer explorer-dispatched",
    );
    builder = builder.header(axum::http::header::ACCEPT, "application/json");
    let mut request = builder.body(Body::empty()).map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("the dispatched request could not be built: {error}"),
        )
    })?;
    request.extensions_mut().insert(token);
    request.extensions_mut().insert(call);
    Ok(request)
}

// ---------------------------------------------------------------------------------------------
// Resolution: pure, and unit-tested without a server
// ---------------------------------------------------------------------------------------------

/// A call resolved into the two strings the router and the snippets need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The path with its `{placeholders}` substituted, always starting with `/`.
    pub path: String,
    /// The query string without the leading `?`, sorted, or empty.
    pub query: String,
    /// The metered surface route: the template with the API's own mount point removed.
    pub surface: String,
    /// The token row's id, so the answer can echo it without threading it separately.
    token_id: Uuid,
    /// The token's display name.
    token_name: String,
    /// The token's `omn_xxxxxxxx` marker.
    token_prefix: String,
    /// The token's own budget.
    rate_limit_per_minute: i32,
}

impl Resolved {
    /// Path and query as one request target.
    pub fn url(&self) -> String {
        if self.query.is_empty() {
            self.path.clone()
        } else {
            format!("{}?{}", self.path, self.query)
        }
    }
}

/// Validate the caller's parameters against the document and build the request target.
///
/// Split out of the handler and free of any `AppState` so the rules it holds — the ones that
/// decide whether a value can leave the segment it was written for — are testable without a
/// database, a server or a token. The token's identity rides along as fields because `Resolved`
/// is *this call's* description, and the answer has to be able to name the token it used.
pub fn resolve(
    endpoint: &Endpoint,
    params: &BTreeMap<String, String>,
    token: &AuthenticatedToken,
) -> Result<Resolved, ApiError> {
    if params.len() > MAX_PARAMS {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_parameter",
            format!("at most {MAX_PARAMS} parameters may be sent"),
        )
        .with_details(json!({ "field": "params" })));
    }

    let path_params = path_parameters(endpoint);
    let mut query_pairs: Vec<(String, String)> = Vec::new();
    for (name, raw) in params {
        let on_path = path_params.contains(&name.as_str());
        let declared =
            on_path || endpoint.parameters.iter().any(|(declared, _)| *declared == name);
        if !declared {
            // An unknown name is refused rather than dropped. Silently ignoring `limitt=5` tells
            // the caller their filter worked, which is worse than a 400 in every way.
            let accepted: Vec<&str> = path_params
                .iter()
                .copied()
                .chain(endpoint.parameters.iter().map(|(name, _)| *name))
                .collect();
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_parameter",
                format!("this endpoint does not take a parameter called \"{name}\""),
            )
            .with_details(json!({ "field": name, "accepted": accepted })));
        }
        if raw.len() > MAX_PARAM_VALUE {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_parameter",
                format!(
                    "\"{name}\" is longer than the {MAX_PARAM_VALUE} bytes a parameter may carry"
                ),
            )
            .with_details(json!({ "field": name })));
        }
        if !on_path {
            query_pairs.push((name.clone(), encode(raw)));
        }
    }

    let path = splice(endpoint.path, &path_params, params)?;
    // Sorted so the resolved URL is a function of the values and not of the iteration order of a
    // hash map: two calls with the same parameters produce the same snippet, so "copy this and
    // paste it into a shell" means what it says.
    query_pairs.sort();
    let query = query_pairs
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&");

    Ok(Resolved {
        path,
        query,
        surface: crate::routes::content_read::surface_route(endpoint.path),
        token_id: token.token.id,
        token_name: token.token.name.clone(),
        token_prefix: token.token.prefix.clone(),
        rate_limit_per_minute: token.token.rate_limit_per_minute,
    })
}

/// The placeholder names in a path template, in template order.
fn path_parameters(endpoint: &Endpoint) -> Vec<&str> {
    let mut names = Vec::new();
    let mut rest = endpoint.path;
    while let Some(open) = rest.find('{') {
        let Some(offset) = rest[open..].find('}') else {
            break;
        };
        names.push(&endpoint.path[open + 1..open + offset]);
        rest = &rest[open + offset + 1..];
    }
    names
}

/// Fill a path template, encoding each value into its own segment.
///
/// Written as a walk rather than a `replace` loop on purpose. A loop that searches for
/// `{name}` in the accumulated output mixes up the placeholders when two of them share a prefix
/// (`{site_id}` and `{site_id_parent}`): the shorter name is found inside the longer, the output
/// is spliced at the wrong offset, and the resulting URL still looks plausible.
fn splice(
    template: &str,
    path_params: &[&str],
    params: &BTreeMap<String, String>,
) -> Result<String, ApiError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(offset) = rest[open..].find('}') else {
            // A template with an unclosed brace is a bug in this table, not in the caller's
            // request. Answering it as a parameter error would blame the caller for our typo.
            out.push_str(&rest[open..]);
            return Ok(out);
        };
        let name = &rest[open + 1..open + offset];
        rest = &rest[open + offset + 1..];
        if !path_params.contains(&name) {
            out.push('{');
            out.push_str(name);
            out.push('}');
            continue;
        }
        let Some(value) = params.get(name) else {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_parameter",
                format!("\"{name}\" is part of this endpoint's path and must be given"),
            )
            .with_details(json!({ "field": name })));
        };
        out.push_str(&encode(value));
    }
    out.push_str(rest);
    Ok(out)
}

/// Percent-encode a value with the unreserved set.
fn encode(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, UNRESERVED).collect()
}

// ---------------------------------------------------------------------------------------------
// Presentation
// ---------------------------------------------------------------------------------------------

/// The headers a caller branches on, in a fixed order.
///
/// Fixed rather than alphabetical so the pane reads the same on every call: `etag` and
/// `cache-control` are the cache story, `x-ratelimit-*` is the budget, `retry-after` only exists
/// on a refusal.
fn interesting_headers(headers: &axum::http::HeaderMap) -> Vec<HeaderPair> {
    const NAMES: [&str; 6] = [
        "content-type",
        "etag",
        "cache-control",
        "retry-after",
        "x-ratelimit-limit",
        "x-ratelimit-remaining",
    ];
    NAMES
        .iter()
        .filter_map(|name| {
            headers.get(*name).and_then(|value| value.to_str().ok()).map(|value| {
                HeaderPair {
                    name: (*name).to_owned(),
                    value: value.to_owned(),
                }
            })
        })
        .collect()
}

/// The call in three languages, all built from one resolved URL.
///
/// **`$OMNION_TOKEN` and not a credential.** The panel cannot produce a token's plaintext, so a
/// snippet carrying one would be a lie — and a lie an integrator pastes into a shell, where it
/// lands in their history. The variable is the honest form: it is what the person pasting the
/// snippet already holds, and the response it produces is the response the pane is showing.
fn snippets_of(method: &str, url: &str) -> Snippets {
    let quoted = shell_single_quote(url);
    Snippets {
        curl: format!(
            "curl -X {method} {quoted} \\\n  -H 'accept: application/json' \\\n  -H \
             'authorization: Bearer $OMNION_TOKEN'"
        ),
        fetch: format!(
            "const response = await fetch({quoted}, {{\n  headers: {{\n    \
             \"accept\": \"application/json\",\n    \"authorization\": \
             \"Bearer \" + process.env.OMNION_TOKEN,\n  }},\n}});\nconst body = \
             await response.json();"
        ),
        python: format!(
            "import os\nimport requests\n\nresponse = requests.get(\n    {quoted},\n    \
             headers={{\"authorization\": f\"Bearer {{os.environ['OMNION_TOKEN']}}\"}},\n)\nprint\
             (response.status_code, response.json())"
        ),
    }
}

/// Quote a value for a POSIX shell, and double-quote it for a JavaScript literal.
///
/// Two quote styles because the two snippets have different readers: a shell swallows an
/// unquoted `?`, and a JavaScript literal must not contain a raw newline. Single quotes make
/// every other character literal in a shell, which is what a URL needs — and the one character
/// that cannot appear inside them is itself, so it is closed, escaped and reopened. Skipping that
/// step is how a URL containing an apostrophe produces a snippet that runs the wrong command.
fn shell_single_quote(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n");
    format!("'{}'", escaped.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    fn endpoint(id: &str) -> &'static Endpoint {
        ENDPOINTS
            .iter()
            .find(|endpoint| endpoint.id == id)
            .expect("the endpoint is documented")
    }

    fn token() -> AuthenticatedToken {
        AuthenticatedToken {
            token: api_tokens::ApiToken {
                id: Uuid::from_u128(0x51),
                organization_id: Uuid::from_u128(1),
                site_id: None,
                name: "QA frontend".into(),
                prefix: "omn_00000000".into(),
                scopes: vec!["content:read".into()],
                allowed_origins: vec![],
                rate_limit_per_minute: 120,
                expires_at: None,
                revoked_at: None,
                last_used_at: None,
                created_by: None,
                created_at: OffsetDateTime::now_utc(),
            },
        }
    }

    fn params(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn a_list_call_resolves_to_a_query_and_the_documented_surface_route() {
        let resolved = resolve(endpoint("pages.list"), &params(&[("limit", "5")]), &token())
            .expect("resolves");
        assert_eq!(resolved.path, "/api/v1/content/pages");
        assert_eq!(resolved.query, "limit=5");
        assert_eq!(resolved.url(), "/api/v1/content/pages?limit=5");
        // The metered row is the TEMPLATE with the mount point off — one endpoint, one row,
        // however many slugs a caller walks.
        assert_eq!(resolved.surface, "/content/pages");
    }

    #[test]
    fn an_empty_call_sends_no_question_mark() {
        let resolved =
            resolve(endpoint("pages.list"), &BTreeMap::new(), &token()).expect("resolves");
        assert_eq!(resolved.url(), "/api/v1/content/pages");
    }

    #[test]
    fn the_query_is_sorted_so_the_same_values_produce_the_same_url() {
        let first =
            resolve(endpoint("pages.list"), &params(&[("limit", "5"), ("locale", "tr")]), &token())
                .expect("resolves");
        let second =
            resolve(endpoint("pages.list"), &params(&[("locale", "tr"), ("limit", "5")]), &token())
                .expect("resolves");
        assert_eq!(first.url(), second.url());
        assert_eq!(first.query, "limit=5&locale=tr");
    }

    #[test]
    fn a_parameter_nobody_declared_is_refused_and_the_accepted_ones_are_named() {
        let error =
            resolve(endpoint("pages.list"), &params(&[("limitt", "5")]), &token())
                .expect_err("a typo must not be ignored");
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            error.details().and_then(|d| d.get("field")).and_then(Value::as_str),
            Some("limitt")
        );
        let accepted = error
            .details()
            .and_then(|d| d.get("accepted"))
            .and_then(Value::as_array)
            .expect("the accepted names ride along");
        assert!(accepted.contains(&Value::from("limit")));
    }

    #[test]
    fn a_path_parameter_that_is_not_given_is_refused_naming_it() {
        let error = resolve(endpoint("pages.read"), &BTreeMap::new(), &token())
            .expect_err("the slug is part of the path");
        assert_eq!(
            error.details().and_then(|d| d.get("field")).and_then(Value::as_str),
            Some("slug")
        );
    }

    #[test]
    fn a_slug_cannot_leave_the_segment_it_belongs_to() {
        // The single most important property here: a slug is user data, and a slug carrying a
        // slash, a question mark or a fragment would otherwise rewrite the URL it is sent in.
        let resolved = resolve(
            endpoint("pages.read"),
            &params(&[("slug", "a/b?x=1#frag"), ("locale", "tr")]),
            &token(),
        )
        .expect("resolves");
        assert_eq!(
            resolved.path, "/api/v1/content/pages/a%2Fb%3Fx%3D1%23frag",
            "the encoded value stays inside its own segment"
        );
        assert!(resolved.query.contains("locale=tr"));
    }

    #[test]
    fn a_newline_in_a_parameter_is_encoded_rather_than_injected() {
        let resolved =
            resolve(endpoint("pages.list"), &params(&[("slug", "a\nb")]), &token())
                .expect("resolves");
        assert!(!resolved.path.contains('\n'));
        assert!(!resolved.query.contains('\n'));
        assert!(resolved.query.contains("%0A"));
    }

    #[test]
    fn a_space_is_encoded_in_the_query_rather_than_left_raw() {
        let resolved = resolve(endpoint("pages.list"), &params(&[("type", "blog post")]), &token())
            .expect("resolves");
        assert!(!resolved.query.contains(' '), "a raw space breaks a query string");
        assert_eq!(resolved.query, "type=blog%20post");
    }

    #[test]
    fn an_ampersand_in_a_value_does_not_fork_the_query() {
        let resolved =
            resolve(endpoint("pages.list"), &params(&[("slug", "a&limit=999")]), &token())
                .expect("resolves");
        assert_eq!(
            resolved.query, "slug=a%26limit%3D999",
            "the value must not be able to inject a second parameter"
        );
    }

    #[test]
    fn an_over_long_parameter_is_refused_before_the_database_ever_sees_it() {
        let huge = "x".repeat(MAX_PARAM_VALUE + 1);
        let error = resolve(endpoint("pages.list"), &params(&[("slug", huge.as_str())]), &token())
            .expect_err("must be refused");
        assert_eq!(
            error.details().and_then(|d| d.get("field")).and_then(Value::as_str),
            Some("slug")
        );
    }

    #[test]
    fn the_authorization_header_is_not_a_parameter_the_form_offers() {
        // It is declared on the document as a *header* and the platform supplies it, so it must
        // not also appear in the query parameter list — a form field for it would invite an
        // operator to paste the one credential this installation cannot produce twice.
        let names: Vec<&str> = endpoint("pages.list")
            .parameters
            .iter()
            .map(|(name, _)| *name)
            .collect();
        assert!(!names.contains(&"Authorization"));
    }

    #[test]
    fn the_resolved_token_is_the_one_the_call_acted_as() {
        let resolved =
            resolve(endpoint("sites.list"), &BTreeMap::new(), &token()).expect("resolves");
        assert_eq!(resolved.token_id, Uuid::from_u128(0x51));
        assert_eq!(resolved.token_name, "QA frontend");
        assert_eq!(resolved.token_prefix, "omn_00000000");
        assert_eq!(resolved.rate_limit_per_minute, 120);
    }

    #[test]
    fn a_snippet_carries_no_credential_and_names_the_variable_instead() {
        let snippets = snippets_of("GET", "https://example.test/api/v1/content/pages?limit=5");
        for text in [&snippets.curl, &snippets.fetch, &snippets.python] {
            assert!(text.contains("OMNION_TOKEN"), "the variable must be named");
            assert!(!text.contains("omn_"), "no credential may appear");
        }
        assert!(snippets.curl.starts_with("curl -X GET 'https://example.test"));
    }

    #[test]
    fn a_url_with_an_apostrophe_produces_a_runnable_snippet() {
        // The escaped form: the quote is closed, a literal quote is written, and the string is
        // reopened. Without it the shell sees an unterminated string and the snippet fails on
        // exactly the URLs a person pastes out of a real response.
        let quoted = shell_single_quote("https://example.test/a'b?c=1");
        assert_eq!(quoted, r#"'https://example.test/a'\''b?c=1'"#);
    }

    #[test]
    fn a_documented_path_already_carries_the_mount_point_and_the_url_adds_none() {
        // **This is the doubled-`/api/v1` bug, pinned.** The document's paths must carry the mount
        // point, because that is what a generated client needs; the caller's URL is therefore the
        // ORIGIN plus the document's path and nothing else. Composing it as origin + `/api/v1` +
        // path produced `https://host/api/v1/api/v1/content/pages` — a snippet an integrator pastes
        // and gets a `404` from, and a resolved-URL line that disagrees with the request that
        // actually went out. Both halves are one URL; nothing else may be added to it.
        for endpoint in ENDPOINTS {
            assert!(
                endpoint.path.starts_with("/api/v1/"),
                "{} does not carry the mount point, so the document is wrong",
                endpoint.id
            );
            // Every placeholder is filled, because the point of this test is the mount point —
            // an item endpoint refusing an empty call is the `400` a different test pins, and a
            // test that quietly got `expect("resolves")` instead would prove nothing here.
            let mut params: BTreeMap<String, String> = BTreeMap::new();
            for name in path_parameter_names(endpoint.path) {
                params.insert(name.to_owned(), "probe".to_owned());
            }
            let resolved = resolve(endpoint, &params, &token()).expect("resolves");
            // The dispatched target is the documented path with its placeholders FILLED IN, so
            // the comparison is against the template's own static prefix — which is what proves
            // no path segment was invented and none was dropped. Comparing against the raw
            // template would be wrong for the item endpoints, whose `slug` is necessarily gone.
            let prefix = static_prefix(endpoint.path);
            assert!(
                resolved.path.starts_with(&prefix),
                "{}: dispatched {} must extend the documented prefix {}",
                endpoint.id,
                resolved.path,
                prefix
            );
            let url = format!("https://example.test{}", resolved.url());
            assert_eq!(
                url.matches("/api/v1").count(),
                1,
                "exactly one mount point in {url}"
            );
        }
    }

    /// The part of a path template before its first `{`.
    ///
    /// Everything a dispatched request must share with the document: the mount point and the
    /// resource path. What comes after the first placeholder is the caller's data, which is
    /// exactly the part that is supposed to differ.
    fn static_prefix(path: &str) -> String {
        let end = path.find('{').unwrap_or(path.len());
        path[..end].to_owned()
    }

    /// The `{placeholder}` names in a path template.
    fn path_parameter_names(path: &str) -> Vec<&str> {
        let mut names = Vec::new();
        let mut rest = path;
        while let Some(open) = rest.find('{') {
            let Some(offset) = rest[open..].find('}') else {
                break;
            };
            names.push(&path[open + 1..open + offset]);
            rest = &rest[open + offset + 1..];
        }
        names
    }

    #[test]
    fn an_unknown_operation_lists_the_documented_ones_rather_than_404ing() {
        let error = endpoint_of("page.list").expect_err("a typo must be refused");
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        let accepted = error
            .details()
            .and_then(|d| d.get("accepted"))
            .and_then(Value::as_array)
            .expect("the documented ids ride along");
        assert!(accepted.contains(&Value::from("pages.list")));
    }
}
