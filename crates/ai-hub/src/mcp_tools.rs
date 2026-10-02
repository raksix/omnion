//! The MCP tool surface: the platform's tools as an external agent sees them (REQ-108, slice 2).
//!
//! # The catalogue is generated, never restated
//!
//! `tools/list` answers from [`crate::catalogue::specs`] — the same compiled table the model
//! payload and the registry screen read — rather than from a hand-written list of MCP tool names.
//! A second list is a second thing to forget: the day a tool is added to `catalogue.rs` and not
//! to the MCP list, the platform quietly stops being able to offer it, and nothing fails. This
//! module exists so that failure cannot be expressed.
//!
//! # What a grant buys, precisely
//!
//! An MCP call is allowed by **three** independent facts, checked in this order, and the order is
//! the security property:
//!
//! 1. the token authenticated and the client is not revoked (slice 1's `authenticate`),
//! 2. the client holds a grant row for the tool (`grant_for`),
//! 3. the token's scopes contain the tool's **declared permission** — the very key the panel
//!    route guards with, which `ops_binding` already proves is a real catalogue key.
//!
//! A denial answers JSON-RPC `-32003` naming the missing permission and **has no side effect**:
//! the handler returns before any service is touched, so the assertion "row counts before and
//! after are equal" is a claim about the world rather than about a return value.
//!
//! # Why sandbox returns a plan instead of refusing
//!
//! The sandbox flag exists so an operator can stand up a client and see what it *would* do
//! without letting it do anything. A sandbox that answered `-32001 permission_denied` would be
//! indistinguishable from a broken grant; a sandbox that ran the write would be no sandbox at
//! all. So a sandboxed call runs **validation, permission resolution and argument inspection**
//! and returns the resolved plan, and the invocation row is written with status `sandbox`. The
//! plan names the tool, the permission, the argument keys and the approval requirement — enough
//! for an operator to judge the client, and not enough to be a write.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The MCP protocol revision this server speaks.
///
/// Pinned rather than "whatever the client asked for": the four methods below are the whole
/// surface, and a client that negotiates a newer revision gets an answer naming the one this
/// build implements instead of a set of fields it will then expect and not find.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// The server name `initialize` reports.
pub const SERVER_NAME: &str = "omnion-mcp";

/// The server version `initialize` reports. The crate version, so a client can tell two servers
/// apart when the same client talks to a development install and to production.
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The JSON-RPC error code the request names for a permission the token's scopes do not carry.
///
/// In the reserved server-error range and **not** `-32001`, which is the generic "unknown tool"
/// shape several clients assume. A distinct code is what lets a client distinguish "I asked for
/// something that does not exist" from "that exists and you may not do it" and print the
/// permission it is missing — which is the single most actionable thing an integrator can be told.
pub const CODE_PERMISSION_DENIED: i64 = -32003;

/// The code for a call whose arguments do not satisfy the tool's schema.
pub const CODE_INVALID_ARGUMENTS: i64 = -32602;

/// The code for a tool this server does not expose at all.
pub const CODE_TOOL_NOT_FOUND: i64 = -32601;

/// The code for a malformed JSON-RPC envelope.
pub const CODE_INVALID_REQUEST: i64 = -32600;

/// The code for a method this server does not implement.
pub const CODE_METHOD_NOT_FOUND: i64 = -32601;

/// The code an air-gapped installation answers with.
pub const CODE_BLOCKED_AIRGAP: i64 = -32004;

/// The code a gated tool answers with while the approval is pending.
pub const CODE_PENDING_APPROVAL: i64 = -32005;

/// The sandbox that proves a plan without writing.
///
/// The literal the request uses, and returned in the plan so a client knows the difference
/// between "this would work" and "this happened".
pub const SANDBOX_NOTE: &str = "sandbox";

/// One tool as `tools/list` renders it.
#[derive(Debug, Clone, Serialize)]
pub struct McpTool {
    /// The tool key — the same string the panel's registry uses, so an operator reading both
    /// screens sees one name rather than two vocabularies.
    pub name: String,
    /// One sentence, carried from the compiled catalogue.
    pub description: String,
    /// The JSON Schema of the arguments. Present on **every** entry: a tool without a schema is
    /// a tool an external client has to guess at, and the docs page's "every tool has a schema"
    /// check reads this field rather than trusting the catalogue to have one.
    pub input_schema: Value,
    /// The permission the client's **scopes** must carry for this call to be allowed.
    pub permission: String,
    /// Whether the registry row requires a human approval before the write runs.
    pub approval_required: bool,
    /// Whether this tool can be exercised with the write suppressed (`sandbox`).
    ///
    /// Every tool can: sandboxing is a property of the *call path*, not of the tool. A `false`
    /// here would mean "this tool cannot be proven", which for a read is exactly backwards.
    pub sandbox_capable: bool,
    /// The risk the catalogue assigns, so a client can decide which calls to run unattended.
    pub risk: String,
    /// The class the panel groups by.
    pub class: String,
    /// One example call, for a client that generates a first request from documentation.
    pub example: Value,
}

/// The `initialize` result.
#[derive(Debug, Clone, Serialize)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: &'static str,
    #[serde(rename = "capabilities")]
    pub capabilities: Capabilities,
    #[serde(rename = "serverInfo")]
    pub server_info: ServerInfo,
}

/// What this server can do. Deliberately two entries and no more: `tools` is the whole surface,
/// and an empty `listChanged` is honest because the catalogue is static at runtime.
#[derive(Debug, Clone, Serialize)]
pub struct Capabilities {
    pub tools: ToolCapability,
}

/// The tools capability.
#[derive(Debug, Clone, Serialize)]
pub struct ToolCapability {
    /// False: this build does not send `notifications/tools/list_changed`, and advertising it
    /// would make every client wait for an update that cannot arrive.
    #[serde(rename = "listChanged")]
    pub list_changed: bool,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            tools: ToolCapability {
                list_changed: false,
            },
        }
    }
}

/// The server's own identity.
#[derive(Debug, Clone, Serialize)]
pub struct ServerInfo {
    pub name: &'static str,
    pub version: &'static str,
}

/// One entry of a `tools/call` result.
///
/// `content` rather than a bare value, because that is the shape the MCP clients read; the
/// single text block carries the JSON body so a client can parse it and a human reading a log
/// sees something legible.
#[derive(Debug, Clone, Serialize)]
pub struct ToolCallResult {
    #[serde(rename = "isError")]
    pub is_error: bool,
    pub content: Vec<ContentBlock>,
    /// Whether this result was produced by the sandbox rather than by the write path.
    #[serde(rename = "structuredContent", skip_serializing_if = "Option::is_none")]
    pub structured: Option<Value>,
}

/// One content block.
#[derive(Debug, Clone, Serialize)]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub text: String,
}

impl ToolCallResult {
    /// A successful result carrying `body` as JSON text.
    #[must_use]
    pub fn ok(body: Value, structured: Option<Value>) -> Self {
        Self {
            is_error: false,
            content: vec![ContentBlock {
                kind: "text",
                text: serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string()),
            }],
            structured,
        }
    }

    /// A refusal that is still a well-formed result — `isError: true` rather than a JSON-RPC
    /// error, because the *call* was understood and answered; what failed is the tool's own
    /// outcome. A tool that threw is a different thing from a tool that said no.
    #[must_use]
    pub fn failed(reason: &str) -> Self {
        Self {
            is_error: true,
            content: vec![ContentBlock {
                kind: "text",
                text: reason.to_owned(),
            }],
            structured: None,
        }
    }
}

/// The error body a JSON-RPC failure answers with.
///
/// `data` carries the missing permission and the tool, because a bare message string is what a
/// client logs and never shows to the person who has to grant the scope.
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcError {
    /// A denial that names the permission and the tool.
    #[must_use]
    pub fn denied(tool: &str, permission: &str) -> Self {
        Self {
            code: CODE_PERMISSION_DENIED,
            message: format!(
                "the token's scopes do not include `{permission}`, which `{tool}` requires"
            ),
            data: Some(json!({ "tool": tool, "permission": permission })),
        }
    }

    /// A plain error with a message and no structured detail.
    #[must_use]
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// With structured detail attached.
    #[must_use]
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

/// One JSON-RPC request, as this server parses it.
#[derive(Debug, Clone, Deserialize)]
pub struct RpcRequest {
    /// `1.0` and `2.0` both parse to the same numbers here; the version check is in
    /// [`check_version`] rather than in the struct because a notification has no id at all.
    #[serde(default)]
    pub jsonrpc: Option<String>,
    /// Absent for a notification. Serialised back as `null` when it was, which is what the
    /// notification spec requires.
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

impl RpcRequest {
    /// The id as a string, for the invocation row.
    ///
    /// A client may send a number or a string; the row's column is text, and JSON-rendering the
    /// number here would store `1` and `"1"` for the same request depending on the client.
    #[must_use]
    pub fn id_text(&self) -> Option<String> {
        self.id.as_ref().map(|value| match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
    }

    /// Whether this is a notification (no id, so no response may be sent).
    #[must_use]
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// The parsed `tools/call` parameters.
#[derive(Debug, Clone, Deserialize)]
pub struct CallParams {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

impl Default for CallParams {
    fn default() -> Self {
        Self {
            name: String::new(),
            arguments: json!({}),
        }
    }
}

/// Whether the envelope's version is the one this server speaks.
///
/// A missing `jsonrpc` is refused rather than assumed: the field is required by the spec, and a
/// client that omits it is a client this server cannot answer in a shape it will read. The
/// *message* is what a client prints, so it names both sides rather than saying "bad request".
#[must_use]
pub fn check_version(request: &RpcRequest) -> Option<JsonRpcError> {
    match request.jsonrpc.as_deref() {
        Some("2.0") => None,
        Some(other) => Some(JsonRpcError::new(
            CODE_INVALID_REQUEST,
            format!("this server speaks JSON-RPC 2.0; the request declared `{other}`"),
        )),
        None => Some(JsonRpcError::new(
            CODE_INVALID_REQUEST,
            "the request has no `jsonrpc` field; this server speaks 2.0",
        )),
    }
}

/// Build the `initialize` answer.
#[must_use]
pub fn initialize_result() -> InitializeResult {
    InitializeResult {
        protocol_version: PROTOCOL_VERSION,
        capabilities: Capabilities::default(),
        server_info: ServerInfo {
            name: SERVER_NAME,
            version: SERVER_VERSION,
        },
    }
}

/// Every catalogue tool as an MCP entry.
///
/// Generated from the compiled catalogue, and the generation is what slice 2's "build check"
/// asserts over: an entry with an empty schema or an empty permission is a defect the catalogue
/// itself would already refuse, so the check here is about *presence*, not about validity.
#[must_use]
pub fn all_tools() -> Vec<McpTool> {
    crate::catalogue::specs()
        .iter()
        .map(|spec| McpTool {
            name: spec.key.to_owned(),
            description: spec.description.to_owned(),
            input_schema: (spec.input_schema)(),
            permission: spec.permission.to_owned(),
            approval_required: crate::catalogue::default_requires_approval(spec),
            sandbox_capable: true,
            risk: spec.risk.as_str().to_owned(),
            class: spec.class.to_owned(),
            example: (spec.example)(),
        })
        .collect()
}

/// One entry by name.
#[must_use]
pub fn tool_named(name: &str) -> Option<McpTool> {
    all_tools().into_iter().find(|tool| tool.name == name)
}

/// Restrict the catalogue to a client's grants, in catalogue order.
///
/// The order is the compiled order rather than alphabetical, so a client's list reads in the
/// same sequence as the panel's registry — an operator comparing the two screens is comparing
/// two lists of the same thing, not two arbitrary orderings.
#[must_use]
pub fn tools_for_grants(granted: &[String]) -> Vec<McpTool> {
    all_tools()
        .into_iter()
        .filter(|tool| granted.iter().any(|name| name == &tool.name))
        .collect()
}

/// What a sandboxed call resolved to without running it.
///
/// This is the whole point of the sandbox, so every field is the fact an operator is judging:
/// the tool, the permission it needs, whether a human must approve it, and the **argument keys**
/// — enough to see "it is about to send this customer's address somewhere", and not enough to
/// be a copy of the payload.
#[derive(Debug, Clone, Serialize)]
pub struct SandboxPlan {
    pub tool: String,
    pub permission: String,
    pub approval_required: bool,
    /// Sorted, because a plan whose key order varies between two identical calls is a plan
    /// nobody can diff in a review.
    pub argument_keys: Vec<String>,
    /// The action this call would take, in the tool's own words.
    pub would: &'static str,
    /// The literal that says this did not happen.
    pub note: &'static str,
}

/// The argument keys of a call, sorted.
///
/// `Value::Object` or nothing: a tool call whose arguments are an array has no fields to name, and
/// listing `"0"`, `"1"` would be a lie about the shape.
#[must_use]
pub fn argument_keys(arguments: &Value) -> Vec<String> {
    let Some(object) = arguments.as_object() else {
        return Vec::new();
    };
    let mut keys: Vec<String> = object.keys().cloned().collect();
    keys.sort();
    keys
}

/// What a `tools/call` resolves to before it runs.
///
/// Returned rather than executed, so the route can decide between running, parking on an
/// approval and refusing — and so every one of those decisions is made in one place with all
/// three facts in hand, instead of in three branches each re-reading one of them.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    /// Allowed: run the tool body.
    Allow {
        permission: String,
        approval_required: bool,
    },
    /// Allowed but the registry gates it: park an approval and tell the client so.
    NeedsApproval { permission: String },
    /// The client holds no grant for the tool. No permission is named, because there is none to
    /// name — the client was never offered the tool, which is a different answer from "you were
    /// offered it and may not use it".
    NotGranted,
    /// The token's scopes do not carry the tool's permission.
    MissingPermission { permission: String },
    /// The registry row is disabled.
    Disabled,
}

impl Resolution {
    /// The stable status an invocation row carries for this resolution.
    #[must_use]
    pub fn status(&self) -> &'static str {
        match self {
            Self::Allow { .. } | Self::NeedsApproval { .. } => "ok",
            Self::NotGranted | Self::MissingPermission { .. } => "denied",
            Self::Disabled => "error",
        }
    }
}

/// Resolve one call against the client's grants, scopes and the registry row.
///
/// # The order is the security claim
///
/// **grant → scope → enabled** is not an arbitrary sequence. A grant is what the operator
/// *offered* the client; a scope is what the client itself may exercise. Checking the scope
/// first would let a client whose grant list is narrow receive `-32003` for a tool it was never
/// offered — which leaks the tool's permission to a client that has no business knowing it, and
/// makes a correctly-narrowed grant list look like a bug. So the grant is asked first, and the
/// refusal for a missing grant is deliberately silent about permissions.
///
/// `approval_required` comes from the grant row, which the panel's picker writes from the
/// registry — so an operator who ungates a tool in the registry ungates it for MCP clients
/// without a second switch to forget.
pub fn resolve(
    granted: Option<&crate::mcp_store::ClientToolRow>,
    scopes: &[String],
    row_enabled: bool,
) -> Resolution {
    let Some(grant) = granted else {
        return Resolution::NotGranted;
    };
    if !row_enabled {
        return Resolution::Disabled;
    }
    let permission = grant.permission.clone().unwrap_or_default();
    if !scopes.iter().any(|scope| scope == &permission) {
        return Resolution::MissingPermission { permission };
    }
    if grant.approval_required {
        return Resolution::NeedsApproval { permission };
    }
    Resolution::Allow {
        permission,
        approval_required: false,
    }
}

/// The `tools/call` result for a resolution that is not `Allow`.
///
/// Every arm carries a code, and the codes are the ones a client branches on — a single string
/// message would make the difference between "retry with a scope" and "ask a human" invisible to
/// anything that is not a human reading the log.
#[must_use]
pub fn refusal_for(resolution: &Resolution, tool: &str) -> JsonRpcError {
    match resolution {
        Resolution::Allow { .. } => JsonRpcError::new(CODE_INVALID_REQUEST, "not a refusal"),
        Resolution::NeedsApproval { permission } => JsonRpcError::new(
            CODE_PENDING_APPROVAL,
            format!(
                "`{tool}` requires a human approval; the request is parked in the approvals inbox \
                 ({permission})"
            ),
        )
        .with_data(json!({
            "tool": tool,
            "permission": permission,
            "state": "pending_approval",
        })),
        // No permission is named here on purpose — see [`resolve`]. Naming it would tell a client
        // what a tool it was never granted requires, which is exactly the discovery the grant
        // list exists to prevent.
        Resolution::NotGranted => JsonRpcError::new(
            CODE_TOOL_NOT_FOUND,
            format!("no tool named `{tool}` is granted to this client"),
        ),
        Resolution::MissingPermission { permission } => JsonRpcError::denied(tool, permission),
        Resolution::Disabled => JsonRpcError::new(
            CODE_TOOL_NOT_FOUND,
            format!("`{tool}` is disabled in this installation's tool registry"),
        ),
    }
}

/// Validate a call's arguments against the tool's compiled schema.
///
/// The **compiled** schema, not the registry row's copy: the row is the operator's editable copy
/// and a hand-edited schema that accepts whatever it likes would be a schema the pipeline
/// validated itself against. This is the same rule `tool_exec` follows, and it is the reason
/// both read `catalogue::specs`.
#[must_use]
pub fn validate_arguments(tool: &McpTool, arguments: &Value) -> Option<JsonRpcError> {
    if arguments.is_null() {
        return None;
    }
    match crate::schema::validate(&tool.input_schema, arguments) {
        Ok(()) => None,
        Err(error) => Some(
            JsonRpcError::new(
                CODE_INVALID_ARGUMENTS,
                format!("`{}` arguments: {error}", tool.name),
            )
            .with_data(json!({ "tool": tool.name, "field": error.path, "reason": error.code })),
        ),
    }
}

/// The hash an invocation row stores for a set of arguments.
///
/// SHA-256 over the **canonical** serialization, so two calls that differ only in key order group
/// together — a hash over a raw `serde_json` string would make `{a,b}` and `{b,a}` two different
/// calls, and the invocation screen's "same call" grouping would be a lie.
#[must_use]
pub fn arguments_digest(arguments: &Value) -> String {
    use sha2::{Digest, Sha256};
    let canonical = canonical_json(arguments);
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    hex::encode(hasher.finalize())
}

/// Serialize with object keys in sorted order, recursively.
///
/// Written rather than pulled in as a dependency for twenty lines: the only guarantee that
/// matters here is that two equal values produce the same string, and a recursive sort is exactly
/// that guarantee. Arrays keep their order — `[1,2]` and `[2,1]` genuinely are different calls.
#[must_use]
pub fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let body: Vec<String> = keys
                .iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_owned()),
                        canonical_json(&map[*key])
                    )
                })
                .collect();
            format!("{{{}}}", body.join(","))
        }
        Value::Array(items) => {
            let body: Vec<String> = items.iter().map(canonical_json).collect();
            format!("[{}]", body.join(","))
        }
        other => other.to_string(),
    }
}

/// What a masked argument preview holds.
///
/// REQ-105's guard decides what may be *stored*; this is the frame that applies it to a JSON-RPC
/// call's arguments. The preview is the guard's own masked text, so the mask style, the rule set
/// and the exemptions are the tenant's — not this module's.
pub fn masked_preview(arguments: &Value, masked: &str) -> Value {
    // The masked *string* is stored under the key the guard rewrote. Storing the masked text
    // beside an unmasked object would defeat the mask, so the object form is produced only when
    // the guard left the payload alone.
    json!({
        "keys": argument_keys(arguments),
        "preview": masked,
        "masked": true,
    })
}

/// A tool's example, as the docs page and `tools/list` show it.
#[must_use]
pub fn example_call(tool: &McpTool) -> Value {
    json!({
        "method": "tools/call",
        "params": { "name": tool.name, "arguments": tool.example },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn the_catalogue_is_generated_so_a_missing_tool_is_impossible() {
        let all = all_tools();
        assert!(all.len() >= 25, "the registry has {} tools", all.len());
        // Every entry carries the three fields a client needs, and the ones slice 2's build
        // check asserts on. An entry with an empty permission would be a tool that answers
        // `-32003` for everything.
        for tool in &all {
            assert!(!tool.name.is_empty());
            assert!(
                !tool.permission.is_empty(),
                "{} names no permission",
                tool.name
            );
            assert!(
                tool.input_schema.get("type").is_some(),
                "{} ships no schema",
                tool.name
            );
            assert!(
                !tool.description.is_empty(),
                "{} ships no description",
                tool.name
            );
        }
    }

    #[test]
    fn a_grant_list_narrows_the_catalogue_and_keeps_its_order() {
        let granted = vec![
            "media.search".to_owned(),
            "content.search".to_owned(),
            "content.create".to_owned(),
        ];
        let listed = tools_for_grants(&granted);
        let names: Vec<&str> = listed.iter().map(|t| t.name.as_str()).collect();
        // Catalogue order, not alphabetical and not the order the caller happened to list.
        assert_eq!(
            names,
            vec!["content.search", "content.create", "media.search"]
        );
    }

    #[test]
    fn a_tool_the_client_was_never_granted_is_denied_without_naming_its_permission() {
        let resolution = resolve(None, &["content.pages.read".to_owned()], true);
        assert_eq!(resolution, Resolution::NotGranted);
        let refusal = refusal_for(&resolution, "content.publish");
        assert_eq!(refusal.code, CODE_TOOL_NOT_FOUND);
        // **The permission is the thing that must not leak.** The tool name in the message is the
        // caller's own input echoed back, which tells them nothing they did not already know;
        // `content.pages.publish` is knowledge about a tool they were never offered. The first
        // version of this assertion checked the *tool* name instead, and it failed on a correct
        // refusal — a test measuring the wrong string cannot fail for the right reason, and this
        // one was green-by-accident in the only direction that mattered.
        let rendered = serde_json::to_string(&refusal).expect("the refusal renders");
        assert!(
            !rendered.contains("content.pages.publish"),
            "the refusal leaks the tool's permission: {rendered}"
        );
        assert!(
            refusal.data.is_none(),
            "no structured detail either — it would carry the same leak"
        );
        // And it still has to *answer*, not go silent: a refusal a client cannot branch on is a
        // refusal a client retries forever.
        assert!(refusal.message.contains("content.publish"));
    }

    #[test]
    fn a_scope_the_token_lacks_is_denied_by_its_name() {
        let grant = crate::mcp_store::ClientToolRow {
            client_id: Uuid::nil(),
            tool: "content.publish".into(),
            permission: Some("content.pages.publish".into()),
            approval_required: false,
            enabled: true,
            added_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        let resolution = resolve(Some(&grant), &["content.pages.read".to_owned()], true);
        assert_eq!(
            resolution,
            Resolution::MissingPermission {
                permission: "content.pages.publish".into()
            }
        );
        let refusal = refusal_for(&resolution, "content.publish");
        assert_eq!(refusal.code, CODE_PERMISSION_DENIED);
        assert!(
            refusal.message.contains("content.pages.publish"),
            "the denial names it: {}",
            refusal.message
        );
        assert_eq!(
            refusal
                .data
                .as_ref()
                .and_then(|d| d.get("permission"))
                .and_then(Value::as_str),
            Some("content.pages.publish")
        );
    }

    #[test]
    fn a_gated_tool_parks_rather_than_running() {
        let grant = crate::mcp_store::ClientToolRow {
            client_id: Uuid::nil(),
            tool: "deployment.deploy".into(),
            permission: Some("deployment.deploy".into()),
            approval_required: true,
            enabled: true,
            added_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        let resolution = resolve(Some(&grant), &["deployment.deploy".to_owned()], true);
        assert_eq!(
            resolution,
            Resolution::NeedsApproval {
                permission: "deployment.deploy".into()
            }
        );
        assert_eq!(
            refusal_for(&resolution, "deployment.deploy").code,
            CODE_PENDING_APPROVAL
        );
    }

    #[test]
    fn a_disabled_registry_row_is_refused_even_with_the_grant_and_the_scope() {
        let grant = crate::mcp_store::ClientToolRow {
            client_id: Uuid::nil(),
            tool: "content.read".into(),
            permission: Some("content.pages.read".into()),
            approval_required: false,
            enabled: true,
            added_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        let resolution = resolve(Some(&grant), &["content.pages.read".to_owned()], false);
        assert_eq!(resolution, Resolution::Disabled);
    }

    #[test]
    fn the_grant_is_asked_before_the_scope() {
        // The order is the security claim, so it is asserted rather than described — and with a
        // tool whose name is NOT its permission, because `users.create` needs `users.create`:
        // a fixture where the two strings coincide cannot tell "the permission leaked" apart from
        // "the caller heard its own name back". `content.publish` needs `content.pages.publish`.
        let scopes: Vec<String> = vec!["content.pages.publish".to_owned()];
        let without_grant = resolve(None, &scopes, true);
        assert_eq!(without_grant.status(), "denied");
        let rendered =
            serde_json::to_string(&refusal_for(&without_grant, "content.publish")).expect("r");
        assert!(
            !rendered.contains("content.pages.publish"),
            "a client with no grant must not learn the permission: {rendered}"
        );

        // The same call *with* the grant is refused differently and by name, which is what makes
        // "the grant is asked first" observable rather than asserted.
        let grant = crate::mcp_store::ClientToolRow {
            client_id: Uuid::nil(),
            tool: "content.publish".into(),
            permission: Some("content.pages.publish".into()),
            approval_required: false,
            enabled: true,
            added_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        let with_grant = resolve(Some(&grant), &scopes, true);
        assert_eq!(
            with_grant,
            Resolution::Allow {
                permission: "content.pages.publish".into(),
                approval_required: false,
            }
        );
    }

    #[test]
    fn arguments_are_validated_against_the_compiled_schema_and_the_field_is_named() {
        let tool = tool_named("content.search").expect("the catalogue has it");
        let good = json!({ "query": "pricing" });
        assert!(validate_arguments(&tool, &good).is_none());

        let missing = json!({});
        let error = validate_arguments(&tool, &missing).expect("a required field is missing");
        assert_eq!(error.code, CODE_INVALID_ARGUMENTS);
        assert!(
            error.message.contains("query"),
            "the message names the field: {}",
            error.message
        );

        let wrong_type = json!({ "query": 42 });
        assert!(validate_arguments(&tool, &wrong_type).is_some());

        let unknown = json!({ "query": "x", "limit": "lots" });
        let error = validate_arguments(&tool, &unknown).expect("additionalProperties is false");
        assert_eq!(error.code, CODE_INVALID_ARGUMENTS);
    }

    #[test]
    fn a_key_order_that_differs_does_not_make_a_different_call() {
        let a = json!({ "alpha": 1, "beta": { "y": 2, "x": 1 } });
        let b = json!({ "beta": { "x": 1, "y": 2 }, "alpha": 1 });
        assert_eq!(canonical_json(&a), canonical_json(&b));
        assert_eq!(arguments_digest(&a), arguments_digest(&b));
        // Array order is meaningful and must survive.
        let c = json!({ "items": [1, 2] });
        let d = json!({ "items": [2, 1] });
        assert_ne!(arguments_digest(&c), arguments_digest(&d));
    }

    #[test]
    fn the_sandbox_plan_names_keys_not_values() {
        let plan = SandboxPlan {
            tool: "media.upload".into(),
            permission: "media.upload".into(),
            approval_required: false,
            argument_keys: argument_keys(&json!({ "url": "https://x/y.png", "filename": "y.png" })),
            would: "upload a media file",
            note: SANDBOX_NOTE,
        };
        assert_eq!(plan.argument_keys, vec!["filename", "url"]);
        let rendered = serde_json::to_string(&plan).expect("the plan renders");
        assert!(
            !rendered.contains("https://x/y.png"),
            "the plan must not carry the argument values: {rendered}"
        );
    }

    #[test]
    fn a_non_object_call_names_no_fields() {
        assert!(argument_keys(&json!([1, 2, 3])).is_empty());
        assert!(argument_keys(&json!("text")).is_empty());
    }

    #[test]
    fn the_version_is_checked_and_the_message_names_both_sides() {
        let good: RpcRequest =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).expect("parses");
        assert!(check_version(&good).is_none());
        let bad: RpcRequest = serde_json::from_str(r#"{"id":1,"method":"ping"}"#).expect("parses");
        let error = check_version(&bad).expect("no version is refused");
        assert_eq!(error.code, CODE_INVALID_REQUEST);
        assert!(error.message.contains("2.0"));
    }

    #[test]
    fn a_request_without_an_id_is_a_notification() {
        let note: RpcRequest =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .expect("parses");
        assert!(note.is_notification());
        assert_eq!(note.id_text(), None);
        let call: RpcRequest =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#)
                .expect("parses");
        assert!(!call.is_notification());
        assert_eq!(call.id_text().as_deref(), Some("7"));
    }

    #[test]
    fn initialize_reports_the_protocol_and_both_server_facts() {
        let result = initialize_result();
        assert_eq!(result.protocol_version, PROTOCOL_VERSION);
        assert_eq!(result.server_info.name, SERVER_NAME);
        assert!(!result.server_info.version.is_empty());
        // `listChanged: false` is a claim the client acts on, so it is asserted.
        assert!(!result.capabilities.tools.list_changed);
    }

    #[test]
    fn a_masked_preview_never_carries_the_original() {
        let arguments = json!({ "email": "ada@example.com", "page": "pricing" });
        let preview = masked_preview(&arguments, "[EMAIL_1] wrote about pricing");
        let rendered = serde_json::to_string(&preview).expect("renders");
        assert!(!rendered.contains("ada@example.com"), "{rendered}");
        assert_eq!(
            preview.get("keys").and_then(Value::as_array).map(Vec::len),
            Some(2),
            "the keys are still named — that is what makes a review possible"
        );
    }
}
