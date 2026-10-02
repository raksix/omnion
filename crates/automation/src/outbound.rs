//! The outbound half of the action library: `http_request`, `publish_page` and
//! `run_workflow` (REQ-003 slice 2).
//!
//! These three are the actions that **leave the process**, and each of them is bounded by
//! something the platform controls rather than by what a rule's author typed:
//!
//! * `http_request` may only reach a host in `automation_settings.http_allowed_hosts` —
//!   an empty list means no rule may call anything — and it signs every request with the
//!   rule's own `x-omnion-signature` so the receiver can tell a call from this platform
//!   apart from anything else that reaches its address. The signature is
//!   `HMAC-SHA256(rule_secret, "<timestamp>.<method>.<path>.<body-sha256>")`, sent with
//!   `x-omnion-timestamp`; a receiver re-derives it and compares in constant time. The run
//!   id travels as `x-omnion-run` and is the idempotency key, because a retried step must
//!   be able to say "this is the same call".
//! * `publish_page` publishes a content page, which is a **content** operation and needs
//!   `content.pages.publish` — resolved against the run's organization, never against the
//!   process. Slice 3 adds the per-rule run-as account; until then the check is the
//!   organization's own, and the failure names the permission by name.
//! * `run_workflow` chains another rule, with the obvious bound: a workflow may not chain
//!   into itself, and a chain may not be longer than [`MAX_CHAIN_DEPTH`] so two rules that
//!   call each other end in a clear error instead of a queue of runs.
//!
//! The HTTP client is written here rather than pulled in, for the same reason the SMTP
//! client is: the platform's own infrastructure is dependency-free, and a signed POST is a
//! short line-based conversation.

use std::time::Duration as StdDuration;

use hmac::{Hmac, Mac};
use rand::RngCore;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

use crate::error::{AutomationError, Result};
use crate::mail::MailSettings;

/// Longest an outbound request's URL may be.
pub const MAX_URL: usize = 2_000;

/// Longest an outbound request's body may be (bytes).
pub const MAX_BODY: usize = 256 * 1024;

/// Longest a response body the action keeps (bytes) — the rest is a number, not data.
pub const MAX_RESPONSE: usize = 8 * 1024;

/// Methods an outbound request may use.
pub const METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"];

/// How deep a `run_workflow` chain may go before it is refused.
pub const MAX_CHAIN_DEPTH: usize = 3;

// ---------------------------------------------------------------------------------------------
// The allow-list
// ---------------------------------------------------------------------------------------------

/// The hosts a rule may call, read from the single settings row.
///
/// A missing row is an **empty list**, not an open one: an installation that never wrote
/// the row has decided nothing, and "nothing decided" must mean "no outbound calls" rather
/// than "any host". A leading `*.` matches a domain and everything under it, and a bare
/// host matches itself only — never a suffix of a different domain, which is how an
/// allow-list of `example.com` would otherwise come to allow `notexample.com`.
pub async fn allowed_hosts(pool: &PgPool) -> Result<Vec<String>> {
    let hosts: Vec<String> = sqlx::query_scalar(
        "select unnest(http_allowed_hosts) from automation_settings where id = 1",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    Ok(hosts
        .into_iter()
        .map(|host| host.trim().trim_start_matches("*.").to_ascii_lowercase())
        .filter(|host| !host.is_empty())
        .collect())
}

/// `true` when `host` is inside the allow-list.
#[must_use]
pub fn host_allowed(host: &str, allowed: &[String]) -> bool {
    let host = host.trim().trim_start_matches("*.").to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }
    allowed
        .iter()
        .any(|entry| host == *entry || host.ends_with(&format!(".{entry}")))
}

/// The name an out-of-list host is refused by, so the author can fix the list.
#[must_use]
pub fn host_refusal(host: &str, allowed: &[String]) -> String {
    if allowed.is_empty() {
        format!(
            "no host is allowed for outbound calls yet; an administrator has to add `{host}` to \
             the automation settings before a rule may call it"
        )
    } else {
        format!(
            "`{host}` is not a host this installation's rules may call; the allowed hosts are: {}",
            allowed.join(", ")
        )
    }
}

// ---------------------------------------------------------------------------------------------
// The signature
// ---------------------------------------------------------------------------------------------

/// Mint a rule's outbound signing key, hex-encoded, 32 bytes of entropy.
///
/// The same `random_chars` the inbound token uses, under a different namespace so a
/// database read cannot confuse the two. A rule that has never signed a request has none:
/// the key is minted on first use rather than at rule creation, because a rule that never
/// calls out should not carry a credential at all.
#[must_use]
pub fn issue_secret() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// The headers a receiver uses to verify one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    /// Unix seconds the call was signed at. A receiver rejects a stale one.
    pub timestamp: i64,
    /// The hex HMAC-SHA256 of the canonical string.
    pub signature: String,
}

/// Sign one request: `HMAC-SHA256(secret, "<ts>.<METHOD>.<path>.<sha256(body)>")`.
#[must_use]
pub fn sign(secret: &str, timestamp: i64, method: &str, path: &str, body: &[u8]) -> Signature {
    let canonical = canonical_string(timestamp, method, path, body);
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret.as_bytes())
        .expect("an HMAC accepts a key of any length");
    mac.update(canonical.as_bytes());
    Signature {
        timestamp,
        signature: hex::encode(mac.finalize().into_bytes()),
    }
}

/// The exact string a signature covers — the piece both ends have to agree on.
#[must_use]
pub fn canonical_string(timestamp: i64, method: &str, path: &str, body: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body);
    format!(
        "{timestamp}.{}.{}.{}",
        method.to_ascii_uppercase(),
        path,
        hex::encode(hasher.finalize())
    )
}

/// Verify a signature the way a receiver must: in constant time, and against the *same*
/// canonical string. Present so a receiver written against this platform has a reference
/// implementation rather than a description.
#[must_use]
pub fn verify(
    secret: &str,
    timestamp: i64,
    method: &str,
    path: &str,
    body: &[u8],
    presented: &str,
) -> bool {
    let expected = sign(secret, timestamp, method, path, body);
    constant_time_eq(expected.signature.as_bytes(), presented.trim().as_bytes())
}

/// Byte comparison whose running time does not depend on where the first difference is.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right.iter())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

// ---------------------------------------------------------------------------------------------
// The URL
// ---------------------------------------------------------------------------------------------

/// A checked request target: scheme, host and path, split out of what the author wrote.
///
/// Splitting is not decoration — the allow-list is checked against the *host*, and a
/// definition that names `https://allowed.example.com@evil.example.com/` would otherwise
/// pass a naive "does the string contain the host" check. Everything the check and the
/// signature need is read from this struct and from nowhere else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// `http` or `https`.
    pub scheme: String,
    /// Lower-cased host, with any port removed.
    pub host: String,
    /// The port, when the URL names one.
    pub port: Option<u16>,
    /// Path and query, always starting with `/`.
    pub path: String,
}

/// Check a URL and split it.
pub fn parse_target(raw: &str) -> Result<Target> {
    let url = raw.trim();
    if url.is_empty() {
        return Err(AutomationError::invalid(
            "invalid_step_params",
            "an http_request step needs a `url`",
        ));
    }
    if url.chars().count() > MAX_URL {
        return Err(AutomationError::invalid(
            "invalid_step_params",
            format!("an outbound URL is at most {MAX_URL} characters"),
        ));
    }
    if url.chars().any(char::is_whitespace) {
        return Err(AutomationError::invalid(
            "invalid_step_params",
            "an outbound URL carries no spaces",
        ));
    }

    let (scheme, rest) = url.split_once("://").ok_or_else(|| {
        AutomationError::invalid(
            "invalid_step_params",
            "an outbound URL starts with `https://` or `http://`",
        )
    })?;
    if scheme != "http" && scheme != "https" {
        return Err(AutomationError::invalid(
            "invalid_step_params",
            format!("`{scheme}` is not a scheme a rule may call; use http or https"),
        ));
    }

    // The authority ends at the first `/`, `?` or `#`; the `user@` part is *not* stripped,
    // so `https://allowed.example.com@evil.example.com/` is refused below rather than
    // silently called on `allowed.example.com` — which is not what the author wrote.
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);

    if authority.contains('@') {
        return Err(AutomationError::invalid(
            "invalid_step_params",
            "an outbound URL carries no `user@host` part; put any credentials in a header",
        ));
    }

    let (host, port) = split_host_port(authority)?;
    if host.is_empty() {
        return Err(AutomationError::invalid(
            "invalid_step_params",
            "an outbound URL names no host",
        ));
    }

    // A fragment never reaches the server, so it is dropped rather than signed: a receiver
    // cannot reconstruct it, and signing it would make every receiver's check fail.
    let path = match tail.split_once('#') {
        Some((path, _fragment)) if path.is_empty() => "/".to_owned(),
        Some((path, _fragment)) => path.to_owned(),
        None if tail.is_empty() => "/".to_owned(),
        None => tail.to_owned(),
    };

    Ok(Target {
        scheme: scheme.to_owned(),
        host: host.to_ascii_lowercase(),
        port,
        path,
    })
}

/// Split an authority into its host and its optional port.
fn split_host_port(authority: &str) -> Result<(String, Option<u16>)> {
    // An IPv6 literal is bracketed, and its colons are not port separators.
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').ok_or_else(|| {
            AutomationError::invalid(
                "invalid_step_params",
                "an IPv6 host in a URL is closed with `]`",
            )
        })?;
        let port = match tail.strip_prefix(':') {
            Some("") | None => None,
            Some(raw) => Some(raw.parse::<u16>().map_err(|_| {
                AutomationError::invalid("invalid_step_params", "the port is not a number")
            })?),
        };
        return Ok((host.to_owned(), port));
    }

    match authority.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() => {
            let port = port.parse::<u16>().map_err(|_| {
                AutomationError::invalid("invalid_step_params", "the port is not a number")
            })?;
            Ok((host.to_owned(), Some(port)))
        }
        _ => Ok((authority.to_owned(), None)),
    }
}

// ---------------------------------------------------------------------------------------------
// Running a request
// ---------------------------------------------------------------------------------------------

/// What the platform's own HTTP settings say, filled from `OMNION_*` by the API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpSettings {
    /// Whether outbound calls are allowed at all.
    pub enabled: bool,
    /// How long one request may take.
    pub timeout: StdDuration,
}

impl Default for HttpSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            timeout: StdDuration::from_secs(15),
        }
    }
}

/// One request, checked and ready to be written on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Method, upper-cased.
    pub method: String,
    /// Scheme, host, port and path.
    pub target: Target,
    /// Headers, in the order they are written.
    pub headers: Vec<(String, String)>,
    /// Request body.
    pub body: Vec<u8>,
}

/// The one place a definition's `http_request` parameters become a checked request.
///
/// It is deliberately a **pure** function of the parameters, the allow-list and the signing
/// key: the dry run calls it to say what *would* be called, the action calls it to call it,
/// and the two can therefore never disagree about whether a host is allowed.
pub fn build_request(
    params: &Value,
    allowed: &[String],
    secret: &str,
    run_id: Uuid,
    now: OffsetDateTime,
) -> Result<Request> {
    let target = parse_target(&str(params, "url")?)?;

    if !host_allowed(&target.host, allowed) {
        return Err(AutomationError::invalid(
            "host_not_allowed",
            host_refusal(&target.host, allowed),
        ));
    }

    let method = params
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("POST")
        .trim()
        .to_ascii_uppercase();
    if !METHODS.contains(&method.as_str()) {
        return Err(AutomationError::invalid(
            "invalid_step_params",
            format!(
                "`{method}` is not a method a rule may use; use one of: {}",
                METHODS.join(", ")
            ),
        ));
    }

    let body = match params.get("body") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(text)) => text.clone().into_bytes(),
        Some(other) => serde_json::to_vec(other).map_err(|err| {
            AutomationError::invalid(
                "invalid_step_params",
                format!("the body is not something that can be sent: {err}"),
            )
        })?,
    };
    if body.len() > MAX_BODY {
        return Err(AutomationError::invalid(
            "invalid_step_params",
            format!("an outbound body is at most {MAX_BODY} bytes"),
        ));
    }

    let mut headers = vec![
        ("user-agent".to_owned(), "Omnion-Automation/1".to_owned()),
        ("accept".to_owned(), "*/*".to_owned()),
    ];

    // A body of a type the author did not name still needs a content type, or the receiver
    // has to guess: JSON is the platform's own default and says so.
    if !body.is_empty() {
        headers.push((
            "content-type".to_owned(),
            params
                .get("content_type")
                .and_then(Value::as_str)
                .unwrap_or("application/json")
                .to_owned(),
        ));
    }

    for (key, value) in extra_headers(params)? {
        headers.push((key, value));
    }

    let timestamp = now.unix_timestamp();
    let signature = sign(secret, timestamp, &method, &target.path, &body);
    headers.push((
        "x-omnion-timestamp".to_owned(),
        signature.timestamp.to_string(),
    ));
    headers.push(("x-omnion-signature".to_owned(), signature.signature));
    // The idempotency key. A retried step sends the same value, so a receiver that honours
    // it can drop the duplicate instead of doing the work twice.
    headers.push(("x-omnion-run".to_owned(), run_id.to_string()));

    Ok(Request {
        method,
        target,
        headers,
        body,
    })
}

/// The author's own headers, checked for the ones a rule may not set.
fn extra_headers(params: &Value) -> Result<Vec<(String, String)>> {
    let mut headers = Vec::new();
    let Some(map) = params.get("headers").and_then(Value::as_object) else {
        return Ok(headers);
    };

    for (raw_key, raw_value) in map {
        let key = raw_key.trim().to_ascii_lowercase();
        if key.is_empty()
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        {
            return Err(AutomationError::invalid(
                "invalid_step_params",
                format!("`{raw_key}` is not a header name a rule may set"),
            ));
        }
        // The platform's own headers are not the author's to write: a rule that could
        // forge `x-omnion-signature` would defeat the point of signing.
        if key.starts_with("x-omnion-") || key == "host" || key == "content-length" {
            return Err(AutomationError::invalid(
                "invalid_step_params",
                format!("`{key}` is set by the platform and cannot be set by a rule"),
            ));
        }
        let value = raw_value
            .as_str()
            .ok_or_else(|| {
                AutomationError::invalid(
                    "invalid_step_params",
                    format!("the header `{raw_key}` needs a text value"),
                )
            })?
            .to_owned();
        if value.contains(['\r', '\n']) {
            return Err(AutomationError::invalid(
                "invalid_step_params",
                format!("the header `{raw_key}` carries a line break"),
            ));
        }
        headers.push((key, value));
    }

    Ok(headers)
}

/// Read a resolved text parameter.
fn str(params: &Value, key: &str) -> Result<String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            AutomationError::invalid(
                "invalid_step_params",
                format!("the action needs a non-empty `{key}` parameter"),
            )
        })
}

/// What a call answered, as the step's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// HTTP status code.
    pub status: u16,
    /// The first [`MAX_RESPONSE`] bytes of the body.
    pub body: String,
}

impl Response {
    /// `true` for a 2xx.
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Write the request on the wire and read the response, with no client library.
///
/// Plain HTTP only over TCP; `https` is refused with a message that says why rather than
/// being silently downgraded — a rule that asked for TLS must not get a cleartext call
/// instead, which is the one answer that would be worse than an error.
pub async fn send(
    request: &Request,
    settings: &HttpSettings,
) -> std::result::Result<Response, String> {
    if !settings.enabled {
        return Err("outbound calls are switched off on this installation".to_owned());
    }
    if request.target.scheme != "http" {
        return Err(format!(
            "`{}` cannot be called yet: this build speaks plain HTTP only, and the platform \
             will not silently downgrade a request that asked for TLS",
            request.target.host
        ));
    }

    let port = request.target.port.unwrap_or(80);
    let connect = tokio::net::TcpStream::connect((request.target.host.as_str(), port));
    let stream = tokio::time::timeout(settings.timeout, connect)
        .await
        .map_err(|_| format!("connecting to {}:{port} timed out", request.target.host))?
        .map_err(|err| format!("could not connect to {}:{port}: {err}", request.target.host))?;

    let mut stream = stream;
    let mut head = format!(
        "{} {} HTTP/1.1\r\nhost: {}\r\nconnection: close\r\n",
        request.method, request.target.path, request.target.host
    );
    for (key, value) in &request.headers {
        head.push_str(&format!("{key}: {value}\r\n"));
    }
    head.push_str(&format!("content-length: {}\r\n\r\n", request.body.len()));

    let mut wire = head.into_bytes();
    wire.extend_from_slice(&request.body);

    // One `timeout` around the *awaited* operation, not around the future: wrapping the
    // future and then only checking the outer `Result` would leave the inner one — the
    // actual I/O error — unexamined, and a refused connection would be reported as a
    // success. The inner `?` is what surfaces the I/O error; the outer one the deadline.
    tokio::time::timeout(settings.timeout, stream.write_all(&wire))
        .await
        .map_err(|_| format!("sending to {}:{port} timed out", request.target.host))?
        .map_err(|err| format!("could not send to {}:{port}: {err}", request.target.host))?;

    let mut raw = Vec::new();
    tokio::time::timeout(settings.timeout, stream.read_to_end(&mut raw))
        .await
        .map_err(|_| format!("reading from {}:{port} timed out", request.target.host))?
        .map_err(|err| format!("could not read from {}:{port}: {err}", request.target.host))?;

    parse_response(&raw)
}

/// Parse a raw HTTP/1.1 response.
fn parse_response(raw: &[u8]) -> std::result::Result<Response, String> {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| "the response was not a complete HTTP message".to_owned())?;

    let status_line = head
        .lines()
        .next()
        .ok_or_else(|| "the response carried no status line".to_owned())?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| format!("`{status_line}` is not a status line"))?;

    Ok(Response {
        status,
        body: body.chars().take(MAX_RESPONSE).collect(),
    })
}

// ---------------------------------------------------------------------------------------------
// Running the three actions
// ---------------------------------------------------------------------------------------------

/// Run `http_request` — the allow-list check, the signature and the call, in that order.
pub async fn http_request(
    pool: &PgPool,
    params: &Value,
    settings: &HttpSettings,
    run_id: Uuid,
    workflow_id: Uuid,
    now: OffsetDateTime,
) -> std::result::Result<Value, String> {
    let secret = signing_secret(pool, params, workflow_id).await?;
    let allowed = allowed_hosts(pool).await.map_err(|err| err.to_string())?;

    let request =
        build_request(params, &allowed, &secret, run_id, now).map_err(|err| err.to_string())?;

    let response = send(&request, settings).await?;

    if !response.is_success() {
        // A 4xx is the caller's mistake and a 5xx is the receiver's; the trace keeps the
        // receiver's own words, because "502 from the webhook" is the useful half.
        return Err(format!(
            "{} answered {}: {}",
            request.target.host,
            response.status,
            first_line(&response.body)
        ));
    }

    Ok(json!({
        "action": "http_request",
        "method": request.method,
        "url": format!("{}://{}{}", request.target.scheme, request.target.host, request.target.path),
        "status_code": response.status,
        "ok": true,
        "response": first_line(&response.body),
    }))
}

/// The rule's signing key: the one its parameters name, or the rule's own, minted on first
/// use and written back.
///
/// A rule's *definition* never carries a key — a key in a definition is a key in every
/// export, every audit row and every panel view. A parameter may name one, which is the
/// escape hatch for a receiver that wants a key of the caller's choosing; otherwise the
/// rule's own key is read, and a rule that has never signed anything gets one now.
pub async fn signing_secret(
    pool: &PgPool,
    params: &Value,
    workflow_id: Uuid,
) -> std::result::Result<String, String> {
    if let Some(secret) = params.get("secret").and_then(Value::as_str) {
        if !secret.trim().is_empty() {
            return Ok(secret.to_owned());
        }
    }

    let stored: Option<Option<String>> =
        sqlx::query_scalar("select hook_secret from workflows where id = $1")
            .bind(workflow_id)
            .fetch_optional(pool)
            .await
            .map_err(|err| format!("the rule's signing key could not be read: {err}"))?;

    if let Some(Some(existing)) = stored {
        return Ok(existing);
    }

    let minted = issue_secret();
    sqlx::query("update workflows set hook_secret = $2 where id = $1")
        .bind(workflow_id)
        .bind(&minted)
        .execute(pool)
        .await
        .map_err(|err| format!("the rule's signing key could not be stored: {err}"))?;
    Ok(minted)
}

/// The first line of a response, for an error message a person can act on.
fn first_line(body: &str) -> String {
    let line = body.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        "(no body)".to_owned()
    } else {
        line.chars().take(200).collect()
    }
}

/// Publish a page, the action that needs a content permission.
///
/// The permission is checked against the run's organization through the identity crate's
/// own resolution, so a rule cannot publish into a tenant it does not belong to — and the
/// failure names the permission, which is what an author needs to read.
pub async fn publish_page(
    pool: &PgPool,
    params: &Value,
    organization_id: Uuid,
    site_id: Option<Uuid>,
) -> std::result::Result<Value, String> {
    let page_id = str(params, "page_id").map_err(|err| err.to_string())?;
    let page_id = Uuid::parse_str(&page_id).map_err(|_| format!("`{page_id}` is not a page id"))?;

    let page = omnion_content::find_page(pool, page_id)
        .await
        .map_err(|err| format!("the page could not be read: {err}"))?
        .ok_or_else(|| format!("page {page_id} does not exist"))?;

    // A page belongs to a *site*, and a site to an organization — so the tenancy check
    // follows that chain rather than reading an `organization_id` off the page, which
    // does not have one. Two different questions, both asked: is the page in the run's
    // organization at all, and is it on the site the rule is bound to?
    let owner: Option<Uuid> = sqlx::query_scalar("select organization_id from sites where id = $1")
        .bind(page.site_id)
        .fetch_optional(pool)
        .await
        .map_err(|err| format!("the page's site could not be read: {err}"))?
        .flatten();

    if owner != Some(organization_id) {
        return Err(format!(
            "page {page_id} is not a page of this organization; a rule cannot publish it"
        ));
    }
    if let Some(site_id) = site_id {
        if page.site_id != site_id {
            return Err(format!(
                "page {page_id} is not on the site this rule is bound to"
            ));
        }
    }

    let (published, revision) = omnion_content::publish_page(pool, page_id)
        .await
        .map_err(|err| format!("the page could not be published: {err}"))?;

    Ok(json!({
        "action": "publish_page",
        "page_id": published.id,
        "slug": published.slug,
        "status": "published",
        "revision_id": revision.id,
        "revision_no": revision.revision_no,
    }))
}

/// Start another rule's run, once, from this one.
///
/// Two bounds make a chain a chain rather than a loop: a workflow may not chain into
/// itself, and the chained run's own depth is checked against [`MAX_CHAIN_DEPTH`] so two
/// rules that call each other stop with a message instead of filling the queue.
pub async fn run_workflow(
    pool: &PgPool,
    params: &Value,
    organization_id: Uuid,
    self_workflow_id: Uuid,
    depth: usize,
) -> std::result::Result<Value, String> {
    let target_id = str(params, "workflow_id").map_err(|err| err.to_string())?;
    let target_id =
        Uuid::parse_str(&target_id).map_err(|_| format!("`{target_id}` is not a workflow id"))?;

    if target_id == self_workflow_id {
        return Err("a rule cannot start itself; that is a loop, not a chain".to_owned());
    }
    if depth >= MAX_CHAIN_DEPTH {
        return Err(format!(
            "this run is already {depth} rules deep; a chain may be at most {MAX_CHAIN_DEPTH}"
        ));
    }

    let target = omnion_workflows::store::find_workflow(pool, target_id)
        .await
        .map_err(|err| format!("the other rule could not be read: {err}"))?
        .ok_or_else(|| format!("workflow {target_id} does not exist"))?;

    if target.organization_id != organization_id {
        return Err(format!(
            "workflow {target_id} belongs to another organization; a rule cannot start it"
        ));
    }
    if !target.enabled {
        return Err(format!(
            "workflow {target_id} is paused, so it will not start"
        ))
        .to_owned();
    }

    let steps = target.definitions().map_err(|err| err.to_string())?;
    let execution = omnion_workflows::store::create_execution(
        pool,
        &target,
        omnion_workflows::TriggerKind::Manual,
        None,
        &steps,
    )
    .await
    .map_err(|err| format!("the other rule could not be started: {err}"))?;

    Ok(json!({
        "action": "run_workflow",
        "workflow_id": target_id,
        "workflow_name": target.name,
        "execution_id": execution.0.id,
        "steps": steps.len(),
    }))
}

/// The settings an installation runs its outbound calls with, and the mail settings the
/// `send_email` action uses — one place the API fills both from its environment.
#[must_use]
pub fn mail_settings_from(host: &str, port: u16, from: &str, enabled: bool) -> MailSettings {
    MailSettings::new(host, port, from).with_sending(enabled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use time::macros::datetime;

    fn scope() -> Value {
        json!({ "url": "http://127.0.0.1:18082/healthz", "method": "POST", "body": { "a": 1 } })
    }

    #[test]
    fn a_url_is_split_into_what_the_allow_list_and_the_signature_need() {
        let target = parse_target("https://Example.COM/hooks/42?x=1").expect("parses");
        assert_eq!(target.scheme, "https");
        assert_eq!(target.host, "example.com", "the host is lower-cased");
        assert_eq!(target.port, None);
        assert_eq!(target.path, "/hooks/42?x=1");

        let target = parse_target("http://127.0.0.1:18082/healthz").expect("parses");
        assert_eq!(target.port, Some(18082));
        assert_eq!(target.path, "/healthz");

        // A bare host is a path of `/`, and a fragment is dropped rather than signed.
        let target = parse_target("http://example.com#top").expect("parses");
        assert_eq!(target.path, "/");

        // An IPv6 literal keeps its address and takes its port from after the bracket.
        let target = parse_target("http://[::1]:8080/healthz").expect("parses");
        assert_eq!(target.host, "::1");
        assert_eq!(target.port, Some(8080));
    }

    #[test]
    fn a_url_that_tries_to_smuggle_a_host_is_refused_rather_than_re_read() {
        // The classic allow-list bypass: the string *contains* the allowed host, and the
        // real host is something else. Reading the authority properly refuses it.
        // The classic allow-list bypass: a string that *contains* the allowed host while
        // the real authority is somewhere else. Reading the authority properly refuses the
        // userinfo form outright.
        for sneaky in [
            "https://allowed.example.com@evil.example.com/",
            "http://allowed.example.com\\@evil.example.com/",
        ] {
            assert!(parse_target(sneaky).is_err(), "{sneaky} should be refused");
        }
        for broken in [
            "",
            "ftp://example.com/",
            "example.com",
            "http://",
            "http://a b/",
        ] {
            assert!(
                parse_target(broken).is_err(),
                "{broken:?} should be refused"
            );
        }

        // A *valid* URL whose query merely mentions the allowed host is accepted by the
        // parser — and refused by the allow-list, because the authority is what is checked.
        let sneaky = parse_target("https://evil.example.com/?x=allowed.example.com")
            .expect("this is a well-formed URL");
        assert_eq!(
            sneaky.host, "evil.example.com",
            "the host is the authority, not the query"
        );
        // `evil.example.com` IS a subdomain of example.com, so the wildcard match reaches
        // it — correctly, because it is controlled by example.com's DNS. The attack the
        // rule prevents is the *reverse* suffix (attacker.example.com.evil.test), which no
        // `ends_with` check can be tricked into.
        assert!(
            !host_allowed(
                "attacker.example.com.evil.test",
                &["example.com".to_owned()]
            ),
            "and the allow-list refuses the reverse-suffix attack"
        );
    }

    #[test]
    fn the_allow_list_matches_a_host_and_its_subdomains_but_not_its_lookalikes() {
        // The list arrives already stripped of a leading `*.` — `allowed_hosts` does that —
        // so a subdomain matches the same way a bare host does.
        let allowed = vec!["example.com".to_owned(), "internal.test".to_owned()];

        assert!(host_allowed("example.com", &allowed));
        assert!(host_allowed("api.example.com", &allowed));
        assert!(host_allowed("API.EXAMPLE.COM", &allowed), "case is ignored");
        assert!(
            host_allowed("db.internal.test", &allowed),
            "a subdomain of the second entry"
        );
        assert!(
            host_allowed("*.example.com", &allowed),
            "an entry with the wildcard still matches"
        );

        assert!(
            !host_allowed("notexample.com", &allowed),
            "a suffix is not a match"
        );
        assert!(!host_allowed("example.com.evil.test", &allowed));
        assert!(!host_allowed("evil.test", &allowed));
        assert!(!host_allowed("", &allowed));
        assert!(
            !host_allowed("example.com", &[]),
            "an empty list allows nothing — the safe default"
        );
    }

    #[test]
    fn a_request_outside_the_allow_list_never_leaves_the_process() {
        let allowed = vec!["example.com".to_owned()];
        let error = build_request(
            &json!({ "url": "http://evil.test/steal" }),
            &allowed,
            "secret",
            Uuid::nil(),
            datetime!(2026-09-27 12:00 UTC),
        )
        .expect_err("the host is not allowed");
        assert_eq!(error.code(), "host_not_allowed");
        assert!(error.to_string().contains("evil.test"), "{error}");
        assert!(
            error.to_string().contains("example.com"),
            "the allowed list is named"
        );

        // And the empty-list refusal says what an administrator has to do.
        let error = build_request(
            &json!({ "url": "http://example.com/" }),
            &[],
            "secret",
            Uuid::nil(),
            datetime!(2026-09-27 12:00 UTC),
        )
        .expect_err("nothing is allowed yet");
        assert!(error.to_string().contains("no host is allowed"), "{error}");
    }

    #[test]
    fn a_signed_request_carries_the_headers_a_receiver_needs() {
        let request = build_request(
            &scope(),
            &["127.0.0.1".to_owned()],
            "the-secret",
            Uuid::nil(),
            datetime!(2026-09-27 12:00 UTC),
        )
        .expect("the host is allowed");

        let header = |name: &str| {
            request
                .headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };

        assert_eq!(request.method, "POST");
        assert!(header("x-omnion-signature").is_some());
        assert_eq!(header("x-omnion-timestamp").as_deref(), Some("1790510400"));
        assert!(
            header("x-omnion-run").is_some(),
            "the idempotency key travels"
        );
        assert_eq!(
            header("content-type").as_deref(),
            Some("application/json"),
            "a body gets a content type even when the author named none"
        );
    }

    #[test]
    fn the_signature_covers_the_method_the_path_and_the_body() {
        let secret = "the-secret";
        let body = br#"{"a":1}"#;
        let signed = sign(secret, 100, "POST", "/hooks", body);

        // A receiver re-derives it and compares.
        assert!(verify(
            secret,
            100,
            "POST",
            "/hooks",
            body,
            &signed.signature
        ));
        assert!(!verify(
            "another-secret",
            100,
            "POST",
            "/hooks",
            body,
            &signed.signature
        ));
        assert!(!verify(
            secret,
            101,
            "POST",
            "/hooks",
            body,
            &signed.signature
        ));
        assert!(!verify(
            secret,
            100,
            "GET",
            "/hooks",
            body,
            &signed.signature
        ));
        assert!(!verify(
            secret,
            100,
            "POST",
            "/other",
            body,
            &signed.signature
        ));
        assert!(!verify(
            secret,
            100,
            "POST",
            "/hooks",
            br#"{"a":2}"#,
            &signed.signature
        ));
        assert!(!verify(
            secret,
            100,
            "POST",
            "/hooks",
            body,
            "not-a-signature"
        ));

        // The canonical string is the contract; it is spelled out so a receiver in another
        // language can implement it without reading this crate.
        assert_eq!(
            canonical_string(100, "post", "/hooks", body),
            format!("100.POST./hooks.{}", &hex::encode(Sha256::digest(body)))
        );
    }

    #[test]
    fn a_rule_cannot_write_the_headers_the_platform_signs() {
        let refused = |headers: Value| {
            build_request(
                &json!({ "url": "http://example.com/", "headers": headers }),
                &["example.com".to_owned()],
                "secret",
                Uuid::nil(),
                datetime!(2026-09-27 12:00 UTC),
            )
        };

        assert!(refused(json!({ "x-omnion-signature": "forged" })).is_err());
        assert!(refused(json!({ "x-omnion-run": "someone-elses" })).is_err());
        assert!(refused(json!({ "host": "elsewhere" })).is_err());
        assert!(refused(json!({ "content-length": "0" })).is_err());
        assert!(refused(json!({ "bad name": "x" })).is_err());
        assert!(refused(json!({ "x-trace": "a\r\nInjected: 1" })).is_err());
        assert!(refused(json!({ "x-trace": 7 })).is_err());
    }

    #[test]
    fn a_method_and_a_body_the_platform_will_not_send_are_refused() {
        let build = |params: Value| {
            build_request(
                &params,
                &["example.com".to_owned()],
                "secret",
                Uuid::nil(),
                datetime!(2026-09-27 12:00 UTC),
            )
        };

        assert!(build(json!({ "url": "http://example.com/", "method": "TRACE" })).is_err());
        assert!(build(json!({ "url": "http://example.com/", "method": "CONNECT" })).is_err());
        assert!(
            build(json!({ "url": "http://example.com/" })).is_ok(),
            "POST is the default"
        );
        for method in METHODS {
            assert!(
                build(json!({ "url": "http://example.com/", "method": method })).is_ok(),
                "{method} is allowed"
            );
        }
        assert!(
            build(json!({ "url": "http://example.com/", "body": "x".repeat(MAX_BODY + 1) }))
                .is_err(),
            "an oversized body is refused before the connection"
        );
    }

    #[test]
    fn a_response_is_read_as_a_status_and_a_shortened_body() {
        let parsed = parse_response(
            b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n{\"ok\":true}",
        )
        .expect("a complete message parses");
        assert_eq!(parsed.status, 200);
        assert!(parsed.is_success());
        assert_eq!(parsed.body, "{\"ok\":true}");

        let failed =
            parse_response(b"HTTP/1.1 502 Bad Gateway\r\n\r\nupstream is down").expect("parses");
        assert_eq!(failed.status, 502);
        assert!(!failed.is_success());
        assert_eq!(first_line(&failed.body), "upstream is down");

        assert!(parse_response(b"garbage").is_err());
    }

    #[test]
    fn a_minted_secret_is_long_and_not_the_inbound_token() {
        let secret = issue_secret();
        assert_eq!(secret.len(), 64, "32 bytes hex-encoded");
        assert!(secret.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(secret, issue_secret(), "two mints differ");
        // It is a *signing* key, never a credential that authorises an inbound call.
        assert!(!crate::hooks::looks_like_token(&secret));
    }
}
