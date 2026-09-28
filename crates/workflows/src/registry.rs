//! The node and credential registry: the contract every node in the palette obeys
//! (docs/requests/REQ-087, slice 1).
//!
//! Two things live here and the split matters:
//!
//! * **The registry is code.** A [`NodeDefinition`] ships with the release, in a `const`
//!   table, and the API only ever reads it. That is what makes the palette trustworthy: a node
//!   cannot appear in the list because a row was inserted, and a node cannot vanish because a
//!   row was deleted. The database (REQ-087 slice 4) records only what is *installed*.
//! * **A node never documents an orphan field.** [`CredentialDefinition`] is validated against
//!   the nodes that name it, so a credential type whose fields no node uses is a lint failure
//!   rather than a form nobody fills in.
//!
//! The types are deliberately serde-shaped: the same struct is what `GET /api/v1/node-types`
//! returns and what a third-party package manifest deserialises into (REQ-087 slice 4), so a
//! manifest that lints here lints there.
//!
//! ```text
//! NodeDefinition ──names──▶ CredentialTypeRef ──▶ CredentialDefinition
//!        │                                              ▲
//!        └──params_schema.secret_field ──────────────────┘ (never a value)
//! ```

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{Result, WorkflowError};

// ---------------------------------------------------------------------------------------------
// Ports
// ---------------------------------------------------------------------------------------------

/// What a port carries, and what it accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortKind {
    /// The main data path. Every node has exactly one.
    Main,
    /// The error path of a node that can route its own failures.
    Error,
    /// Reserved for the AI tool port (REQ-097…REQ-104). Declared, never executed here.
    AiTool,
}

impl PortKind {
    /// Canonical lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Error => "error",
            Self::AiTool => "ai_tool",
        }
    }

    /// Parse a stored name.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "main" => Some(Self::Main),
            "error" => Some(Self::Error),
            "ai_tool" => Some(Self::AiTool),
            _ => None,
        }
    }
}

/// One input or output port of a node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Port {
    /// Port name; unique within its direction.
    pub name: String,
    /// What the port is.
    pub kind: PortKind,
    /// Data kinds this port accepts, e.g. `["text", "json"]`. Empty means "anything", and
    /// an empty list is *not* a refusal — the mismatch check is one-directional (see
    /// [`NodeDefinition::check_connection`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accepts: Vec<String>,
    /// Whether a connection may leave this port. Only one `main` output may be open.
    #[serde(default = "yes")]
    pub open: bool,
}

/// Serde default for [`Port::open`]: a port exists to be connected.
fn yes() -> bool {
    true
}

// ---------------------------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------------------------

/// How the palette renders one parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamHint {
    /// A one-line text field.
    Text,
    /// A multi-line text field.
    Textarea,
    /// A code field (the palette highlights it, the sandbox runs it — never in the browser).
    Code,
    /// A select whose options come from `options_source` rather than the schema.
    Select,
    /// A number field.
    Number,
    /// A checkbox.
    Boolean,
}

impl ParamHint {
    /// Canonical lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Textarea => "textarea",
            Self::Code => "code",
            Self::Select => "select",
            Self::Number => "number",
            Self::Boolean => "boolean",
        }
    }
}

/// One parameter of a node: a JSON Schema subset plus the hints the palette needs.
///
/// The subset is the four JSON Schema keywords the palette can actually enforce — `type`,
/// `required`, `enum`, `default` — plus `ui`. Anything richer belongs in a node package's own
/// validator (REQ-087 slice 4); a schema the palette cannot render is a schema the person
/// filling the form cannot see.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParamSpec {
    /// Parameter name as it appears in `params`.
    pub name: String,
    /// JSON Schema type: `string`, `number`, `integer`, `boolean` or `object`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Field label in the inspector.
    pub label: String,
    /// Whether the value must be present.
    #[serde(default)]
    pub required: bool,
    /// How the palette renders it.
    #[serde(default = "text_hint")]
    pub ui: ParamHint,
    /// Allowed values for a `ui: select` field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[serde(rename = "enum")]
    pub options: Vec<String>,
    /// For a `ui: select` whose options are fetched (an endpoint, a credential's connected
    /// identity). A `select` without either `enum` or `options_source` is a lint failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options_source: Option<String>,
    /// Placeholder for a text field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// One-line help under the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    /// Default value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    /// Marks a field whose value is a credential *key*, never a secret. The palette renders it
    /// as a credential picker; the lint refuses a `secret` field here, because a node param
    /// that holds a secret is a second secret store (REQ-087 risks).
    #[serde(default)]
    pub secret_field: bool,
}

/// Serde default for [`ParamSpec::ui`]: a plain text field.
fn text_hint() -> ParamHint {
    ParamHint::Text
}

// ---------------------------------------------------------------------------------------------
// Node definition
// ---------------------------------------------------------------------------------------------

/// What a node is for, in the palette's category tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeCategory {
    /// Starts a run.
    Trigger,
    /// Branches, merges, loops.
    Flow,
    /// Runs code in the sandbox.
    Code,
    /// Reshapes items: filter, split, aggregate.
    Data,
    /// Talks to another service.
    Integration,
    /// Small utilities with no side effect.
    Helper,
    /// Routes failures somewhere.
    ErrorHandler,
}

impl NodeCategory {
    /// Canonical lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trigger => "trigger",
            Self::Flow => "flow",
            Self::Code => "code",
            Self::Data => "data",
            Self::Integration => "integration",
            Self::Helper => "helper",
            Self::ErrorHandler => "error_handler",
        }
    }

    /// Parse a stored name.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "trigger" => Some(Self::Trigger),
            "flow" => Some(Self::Flow),
            "code" => Some(Self::Code),
            "data" => Some(Self::Data),
            "integration" => Some(Self::Integration),
            "helper" => Some(Self::Helper),
            "error_handler" => Some(Self::ErrorHandler),
            _ => None,
        }
    }

    /// Every category, in palette order.
    #[must_use]
    pub const fn all() -> [Self; 7] {
        [
            Self::Trigger,
            Self::Flow,
            Self::Code,
            Self::Data,
            Self::Integration,
            Self::Helper,
            Self::ErrorHandler,
        ]
    }
}

/// What a node can do, beyond running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Runs when the engine reaches it.
    Execute,
    /// Polls on a schedule.
    Poll,
    /// Receives inbound HTTP.
    Webhook,
    /// Starts a run.
    Trigger,
}

impl Capability {
    /// Canonical lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Execute => "execute",
            Self::Poll => "poll",
            Self::Webhook => "webhook",
            Self::Trigger => "trigger",
        }
    }

    /// Parse a stored name.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "execute" => Some(Self::Execute),
            "poll" => Some(Self::Poll),
            "webhook" => Some(Self::Webhook),
            "trigger" => Some(Self::Trigger),
            _ => None,
        }
    }
}

/// Whether a node's code runs out of process (docs/09-N8N-TEARDOWN.md §13 lesson 14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sandbox {
    /// No code at all: the engine's built-in actions and host actions only.
    None,
    /// Third-party package code, out of process.
    Required,
}

impl Sandbox {
    /// Canonical lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Required => "required",
        }
    }
}

/// One node in the registry.
///
/// The `Default` is deliberately not derivable: a node declares every field, so a new required
/// one is a compile error in every table rather than a silently-defaulted definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeDefinition {
    /// Stable key, e.g. `http_request`.
    pub key: &'static str,
    /// Version a workflow is recorded against.
    pub version: &'static str,
    /// Label in the palette.
    pub label: &'static str,
    /// One sentence on the detail screen.
    pub description: &'static str,
    /// Category tree placement.
    pub category: NodeCategory,
    /// Lucide icon name, rendered by the admin app. Never an emoji or a coloured blob.
    pub icon: &'static str,
    /// Documentation link.
    pub docs_url: &'static str,
    /// Input ports, in order. The first `main` is the implicit input for a node with one.
    pub inputs: Vec<Port>,
    /// Output ports, in order.
    pub outputs: Vec<Port>,
    /// Parameters the inspector renders.
    pub params: Vec<ParamSpec>,
    /// Credential types this node can use, by key.
    pub credential_types: Vec<&'static str>,
    /// What the node can do.
    pub capabilities: Vec<Capability>,
    /// Whether the node's code runs out of process.
    pub sandbox: Sandbox,
    /// Attempts a run allows when the person setting it up did not say.
    pub default_max_attempts: i32,
    /// Whether this version is deprecated.
    pub deprecated: bool,
    /// The node key that replaces a deprecated one, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<&'static str>,
}

impl NodeDefinition {
    /// The open `main` output ports, in order. A node has at most one; the lint says so.
    #[must_use]
    pub fn main_outputs(&self) -> Vec<&Port> {
        self.outputs
            .iter()
            .filter(|port| port.kind == PortKind::Main && port.open)
            .collect()
    }

    /// `true` when the node starts a run rather than running inside one.
    #[must_use]
    pub fn is_trigger(&self) -> bool {
        self.capabilities.contains(&Capability::Trigger)
    }

    /// Whether a connection from `self` to `to` is legal, and why not when it is not.
    ///
    /// The check is one-directional on purpose: an empty `accepts` on the target port means
    /// "anything" (a caller must never have to enumerate every kind to be connectable), while an
    /// output that declares `accepts` is held to it. Control-flow cycles and self-loops are the
    /// graph's answer, not a port's, and are checked in the compiler.
    pub fn check_connection(&self, output: &str, to: &NodeDefinition, input: &str) -> Result<()> {
        let from_port = self
            .outputs
            .iter()
            .find(|port| port.name == output)
            .ok_or_else(|| {
                WorkflowError::invalid(
                    "connection_port_unknown",
                    format!("node \"{}\" has no output port \"{output}\"", self.key),
                )
            })?;
        let to_port = to
            .inputs
            .iter()
            .find(|port| port.name == input)
            .ok_or_else(|| {
                WorkflowError::invalid(
                    "connection_port_unknown",
                    format!("node \"{}\" has no input port \"{input}\"", to.key),
                )
            })?;

        if !from_port.open {
            return Err(WorkflowError::invalid(
                "connection_port_closed",
                format!("the output port \"{output}\" of \"{}\" is closed", self.key),
            ));
        }
        if from_port.kind == PortKind::Error && to_port.kind != PortKind::Error {
            return Err(WorkflowError::invalid(
                "connection_type_mismatch",
                format!(
                    "the error output \"{output}\" of \"{}\" cannot feed the \"{}\" input of \"{}\"",
                    self.key, input, to.key
                ),
            ));
        }
        if !to_port.accepts.is_empty()
            && !from_port.accepts.is_empty()
            && !from_port
                .accepts
                .iter()
                .any(|kind| to_port.accepts.iter().any(|accepted| accepted == kind))
        {
            return Err(WorkflowError::invalid(
                "connection_type_mismatch",
                format!(
                    "\"{}\" outputs {} but \"{}\" accepts {}",
                    self.key,
                    from_port.accepts.join("/"),
                    to.key,
                    to_port.accepts.join("/")
                ),
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Credential definitions
// ---------------------------------------------------------------------------------------------

/// How a credential field is filled in and stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    /// Plain text.
    String,
    /// A secret: write-only, never returned, always masked (REQ-087 slice 2).
    Secret,
    /// A URL.
    Url,
    /// A number.
    Number,
    /// A checkbox.
    Boolean,
    /// One of a fixed set.
    Select,
}

impl FieldType {
    /// Canonical lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Secret => "secret",
            Self::Url => "url",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Select => "select",
        }
    }

    /// Parse a stored name.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "string" => Some(Self::String),
            "secret" => Some(Self::Secret),
            "url" => Some(Self::Url),
            "number" => Some(Self::Number),
            "boolean" => Some(Self::Boolean),
            "select" => Some(Self::Select),
            _ => None,
        }
    }
}

/// How a credential's secrets are obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    /// A single API key.
    ApiKey,
    /// An OAuth 2.0 authorization-code flow with refresh.
    OAuth2,
    /// Username and password.
    BasicAuth,
    /// An SMTP relay.
    Smtp,
    /// A private key file.
    SshKey,
    /// S3-compatible object storage.
    CloudStorage,
    /// A payment provider.
    Payment,
}

impl CredentialKind {
    /// Canonical lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ApiKey => "api_key",
            Self::OAuth2 => "oauth2",
            Self::BasicAuth => "basic_auth",
            Self::Smtp => "smtp",
            Self::SshKey => "ssh_key",
            Self::CloudStorage => "cloud_storage",
            Self::Payment => "payment",
        }
    }

    /// Parse a stored name.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "api_key" => Some(Self::ApiKey),
            "oauth2" => Some(Self::OAuth2),
            "basic_auth" => Some(Self::BasicAuth),
            "smtp" => Some(Self::Smtp),
            "ssh_key" => Some(Self::SshKey),
            "cloud_storage" => Some(Self::CloudStorage),
            "payment" => Some(Self::Payment),
            _ => None,
        }
    }
}

/// One field of a credential form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialField {
    /// Field name in the submitted payload.
    pub name: &'static str,
    /// Field label.
    pub label: &'static str,
    /// How it is filled in and stored.
    #[serde(rename = "type")]
    pub kind: FieldType,
    /// Whether the form refuses to save without it.
    #[serde(default)]
    pub required: bool,
    /// Allowed values for a `select` field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<&'static str>,
    /// One-line help under the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<&'static str>,
    /// Whether a `secret` field is also kept out of the log. A `secret` field without this is a
    /// lint failure: the field exists so the value never lands in a line of output, and a
    /// secret that is logged is no longer secret.
    #[serde(default)]
    pub never_log: bool,
}

/// The OAuth settings of a credential type (REQ-087 slice 3 writes the flow itself).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthConfig {
    /// Authorization endpoint.
    pub authorize_url: &'static str,
    /// Token endpoint.
    pub token_url: &'static str,
    /// Scopes requested, space-separated.
    pub scopes: &'static str,
    /// Whether the flow uses PKCE. Required for a public client and the default for new ones.
    pub pkce: bool,
    /// Whether the token set is refreshed before expiry.
    pub refresh: bool,
}

/// One credential type in the registry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialDefinition {
    /// Stable key, e.g. `smtp`.
    pub key: &'static str,
    /// How secrets are obtained.
    pub kind: CredentialKind,
    /// Label on the type picker.
    pub label: &'static str,
    /// One sentence on the detail screen.
    pub description: &'static str,
    /// Lucide icon name.
    pub icon: &'static str,
    /// Documentation link.
    pub docs_url: &'static str,
    /// The form's fields, in order.
    pub fields: Vec<CredentialField>,
    /// OAuth settings, for an `oauth2` type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthConfig>,
    /// Seconds the test hook may take before it is abandoned.
    pub test_timeout_seconds: i64,
}

impl CredentialDefinition {
    /// The names of the fields that must not be logged.
    #[must_use]
    pub fn secret_fields(&self) -> Vec<&'static str> {
        self.fields
            .iter()
            .filter(|field| field.kind == FieldType::Secret)
            .map(|field| field.name)
            .collect()
    }
}

// ---------------------------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------------------------

/// A finding of [`lint`]: one thing wrong with one definition, named so a test can assert it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LintFinding {
    /// Stable code, e.g. `node_duplicate_key`.
    pub code: &'static str,
    /// Which definition it is about.
    pub subject: String,
    /// What is wrong, in one sentence.
    pub message: String,
}

/// The read-only registry: every bundled node and credential type, in code.
///
/// A `const` slice, not a `HashMap`, because the order is the palette's order and because a
/// duplicate key has to be a *lint failure* rather than a map that quietly overwrote one.
static NODES: std::sync::LazyLock<Vec<NodeDefinition>> = std::sync::LazyLock::new(|| {
    vec![
        NodeDefinition {
            key: "manual_trigger",
            version: "1.0.0",
            label: "Manual trigger",
            description: "Starts the run when a person presses Run, or when the API asks for it.",
            category: NodeCategory::Trigger,
            icon: "Play",
            docs_url: "https://docs.omnion.dev/nodes/manual-trigger",
            inputs: vec![],
            outputs: vec![Port {
                name: "out".into(),
                kind: PortKind::Main,
                accepts: vec![],
                open: true,
            }],
            params: vec![],
            credential_types: vec![],
            capabilities: vec![Capability::Trigger, Capability::Execute],
            sandbox: Sandbox::None,
            default_max_attempts: 1,
            deprecated: false,
            superseded_by: None,
        },
        NodeDefinition {
            key: "schedule_trigger",
            version: "1.0.0",
            label: "Schedule trigger",
            description: "Starts the run on a cron schedule in UTC.",
            category: NodeCategory::Trigger,
            icon: "Clock",
            docs_url: "https://docs.omnion.dev/nodes/schedule-trigger",
            inputs: vec![],
            outputs: vec![Port {
                name: "out".into(),
                kind: PortKind::Main,
                accepts: vec![],
                open: true,
            }],
            params: vec![ParamSpec {
                name: "cron".into(),
                kind: "string".into(),
                label: "Cron expression".into(),
                required: true,
                ui: ParamHint::Text,
                options: vec![],
                options_source: None,
                placeholder: Some("*/15 * * * *".into()),
                help: Some(
                    "Five fields, UTC. The preview on the trigger screen shows the next fires."
                        .into(),
                ),
                default: None,
                secret_field: false,
            }],
            credential_types: vec![],
            capabilities: vec![Capability::Trigger, Capability::Execute],
            sandbox: Sandbox::None,
            default_max_attempts: 1,
            deprecated: false,
            superseded_by: None,
        },
        NodeDefinition {
            key: "http_request",
            version: "1.0.0",
            label: "HTTP request",
            description: "Calls one HTTP endpoint and turns the response into items.",
            category: NodeCategory::Integration,
            icon: "Globe",
            docs_url: "https://docs.omnion.dev/nodes/http-request",
            inputs: vec![Port {
                name: "in".into(),
                kind: PortKind::Main,
                accepts: vec![],
                open: true,
            }],
            outputs: vec![Port {
                name: "out".into(),
                kind: PortKind::Main,
                accepts: vec!["json".into(), "text".into(), "binary".into()],
                open: true,
            }],
            params: vec![
                ParamSpec {
                    name: "url".into(),
                    kind: "string".into(),
                    label: "URL".into(),
                    required: true,
                    ui: ParamHint::Text,
                    options: vec![],
                    options_source: None,
                    placeholder: Some("https://api.example.com/v1/orders".into()),
                    help: None,
                    default: None,
                    secret_field: false,
                },
                ParamSpec {
                    name: "method".into(),
                    kind: "string".into(),
                    label: "Method".into(),
                    required: true,
                    ui: ParamHint::Select,
                    options: vec![
                        "GET".into(),
                        "POST".into(),
                        "PUT".into(),
                        "PATCH".into(),
                        "DELETE".into(),
                    ],
                    options_source: None,
                    placeholder: None,
                    help: None,
                    default: Some(json!("GET")),
                    secret_field: false,
                },
                ParamSpec {
                    name: "credential_key".into(),
                    kind: "string".into(),
                    label: "Credential".into(),
                    required: false,
                    ui: ParamHint::Select,
                    options: vec![],
                    options_source: Some("credentials".into()),
                    placeholder: None,
                    help: Some("A credential key, never a secret value.".into()),
                    default: None,
                    secret_field: true,
                },
            ],
            credential_types: vec!["api_key", "oauth2", "basic_auth"],
            capabilities: vec![Capability::Execute],
            sandbox: Sandbox::None,
            default_max_attempts: 3,
            deprecated: false,
            superseded_by: None,
        },
        NodeDefinition {
            key: "send_email",
            version: "1.0.0",
            label: "Send email",
            description: "Sends one message through the configured relay.",
            category: NodeCategory::Integration,
            icon: "Mail",
            docs_url: "https://docs.omnion.dev/nodes/send-email",
            inputs: vec![Port {
                name: "in".into(),
                kind: PortKind::Main,
                accepts: vec![],
                open: true,
            }],
            outputs: vec![Port {
                name: "out".into(),
                kind: PortKind::Main,
                accepts: vec!["text".into(), "json".into()],
                open: true,
            }],
            params: vec![
                ParamSpec {
                    name: "to".into(),
                    kind: "string".into(),
                    label: "To".into(),
                    required: true,
                    ui: ParamHint::Text,
                    options: vec![],
                    options_source: None,
                    placeholder: Some("ops@example.com".into()),
                    help: None,
                    default: None,
                    secret_field: false,
                },
                ParamSpec {
                    name: "subject".into(),
                    kind: "string".into(),
                    label: "Subject".into(),
                    required: true,
                    ui: ParamHint::Text,
                    options: vec![],
                    options_source: None,
                    placeholder: None,
                    help: None,
                    default: None,
                    secret_field: false,
                },
                ParamSpec {
                    name: "body".into(),
                    kind: "string".into(),
                    label: "Body".into(),
                    required: true,
                    ui: ParamHint::Textarea,
                    options: vec![],
                    options_source: None,
                    placeholder: None,
                    help: None,
                    default: None,
                    secret_field: false,
                },
                ParamSpec {
                    name: "credential_key".into(),
                    kind: "string".into(),
                    label: "Relay credential".into(),
                    required: true,
                    ui: ParamHint::Select,
                    options: vec![],
                    options_source: Some("credentials".into()),
                    placeholder: None,
                    help: Some("The SMTP credential this message leaves through.".into()),
                    default: None,
                    secret_field: true,
                },
            ],
            credential_types: vec!["smtp"],
            capabilities: vec![Capability::Execute],
            sandbox: Sandbox::None,
            default_max_attempts: 2,
            deprecated: false,
            superseded_by: None,
        },
        NodeDefinition {
            key: "code",
            version: "1.0.0",
            label: "Code",
            description: "Runs a short script over each item in the sandbox.",
            category: NodeCategory::Code,
            icon: "Code2",
            docs_url: "https://docs.omnion.dev/nodes/code",
            inputs: vec![Port {
                name: "in".into(),
                kind: PortKind::Main,
                accepts: vec![],
                open: true,
            }],
            outputs: vec![Port {
                name: "out".into(),
                kind: PortKind::Main,
                accepts: vec!["json".into(), "text".into(), "number".into()],
                open: true,
            }],
            params: vec![
                ParamSpec {
                    name: "language".into(),
                    kind: "string".into(),
                    label: "Language".into(),
                    required: true,
                    ui: ParamHint::Select,
                    options: vec!["javascript".into(), "python".into()],
                    options_source: None,
                    placeholder: None,
                    help: None,
                    default: Some(json!("javascript")),
                    secret_field: false,
                },
                ParamSpec {
                    name: "source".into(),
                    kind: "string".into(),
                    label: "Source".into(),
                    required: true,
                    ui: ParamHint::Code,
                    options: vec![],
                    options_source: None,
                    placeholder: Some("return items.map(i => ({ ...i, seen: true }));".into()),
                    help: Some("Runs out of process. The browser never executes this.".into()),
                    default: None,
                    secret_field: false,
                },
            ],
            credential_types: vec![],
            capabilities: vec![Capability::Execute],
            sandbox: Sandbox::Required,
            default_max_attempts: 1,
            deprecated: false,
            superseded_by: None,
        },
        NodeDefinition {
            key: "if",
            version: "1.0.0",
            label: "If",
            description: "Routes each item down the true or the false branch.",
            category: NodeCategory::Flow,
            icon: "GitBranch",
            docs_url: "https://docs.omnion.dev/nodes/if",
            inputs: vec![Port {
                name: "in".into(),
                kind: PortKind::Main,
                accepts: vec![],
                open: true,
            }],
            outputs: vec![
                Port {
                    name: "true".into(),
                    kind: PortKind::Main,
                    accepts: vec![
                        "json".into(),
                        "text".into(),
                        "number".into(),
                        "boolean".into(),
                    ],
                    open: true,
                },
                Port {
                    name: "false".into(),
                    kind: PortKind::Main,
                    accepts: vec![
                        "json".into(),
                        "text".into(),
                        "number".into(),
                        "boolean".into(),
                    ],
                    open: true,
                },
            ],
            params: vec![ParamSpec {
                name: "condition".into(),
                kind: "string".into(),
                label: "Condition".into(),
                required: true,
                ui: ParamHint::Text,
                options: vec![],
                options_source: None,
                placeholder: Some("{{ $json.total > 100 }}".into()),
                help: Some("An expression, evaluated per item.".into()),
                default: None,
                secret_field: false,
            }],
            credential_types: vec![],
            capabilities: vec![Capability::Execute],
            sandbox: Sandbox::None,
            default_max_attempts: 1,
            deprecated: false,
            superseded_by: None,
        },
        NodeDefinition {
            key: "s3_upload",
            version: "1.0.0",
            label: "S3 object upload",
            description: "Writes each item to an S3-compatible bucket.",
            category: NodeCategory::Integration,
            icon: "CloudUpload",
            docs_url: "https://docs.omnion.dev/nodes/s3-upload",
            inputs: vec![Port {
                name: "in".into(),
                kind: PortKind::Main,
                accepts: vec![],
                open: true,
            }],
            outputs: vec![Port {
                name: "out".into(),
                kind: PortKind::Main,
                accepts: vec!["json".into()],
                open: true,
            }],
            params: vec![
                ParamSpec {
                    name: "bucket".into(),
                    kind: "string".into(),
                    label: "Bucket".into(),
                    required: true,
                    ui: ParamHint::Text,
                    options: vec![],
                    options_source: None,
                    placeholder: Some("assets".into()),
                    help: None,
                    default: None,
                    secret_field: false,
                },
                ParamSpec {
                    name: "key".into(),
                    kind: "string".into(),
                    label: "Object key".into(),
                    required: true,
                    ui: ParamHint::Text,
                    options: vec![],
                    options_source: None,
                    placeholder: Some("{{ $json.id }}.json".into()),
                    help: None,
                    default: None,
                    secret_field: false,
                },
                ParamSpec {
                    name: "credential_key".into(),
                    kind: "string".into(),
                    label: "Credential".into(),
                    required: true,
                    ui: ParamHint::Select,
                    options: vec![],
                    options_source: Some("credentials".into()),
                    placeholder: None,
                    help: Some("A cloud-storage credential key.".into()),
                    default: None,
                    secret_field: true,
                },
            ],
            credential_types: vec!["cloud_storage"],
            capabilities: vec![Capability::Execute],
            sandbox: Sandbox::None,
            default_max_attempts: 3,
            deprecated: false,
            superseded_by: None,
        },
        NodeDefinition {
            key: "filter",
            version: "1.0.0",
            label: "Filter",
            description: "Keeps the items whose expression is true.",
            category: NodeCategory::Data,
            icon: "Filter",
            docs_url: "https://docs.omnion.dev/nodes/filter",
            inputs: vec![Port {
                name: "in".into(),
                kind: PortKind::Main,
                accepts: vec![],
                open: true,
            }],
            outputs: vec![Port {
                name: "out".into(),
                kind: PortKind::Main,
                accepts: vec![
                    "json".into(),
                    "text".into(),
                    "number".into(),
                    "boolean".into(),
                ],
                open: true,
            }],
            params: vec![ParamSpec {
                name: "condition".into(),
                kind: "string".into(),
                label: "Condition".into(),
                required: true,
                ui: ParamHint::Text,
                options: vec![],
                options_source: None,
                placeholder: Some("{{ $json.status === 'paid' }}".into()),
                help: Some("Items whose expression is false are dropped.".into()),
                default: None,
                secret_field: false,
            }],
            credential_types: vec![],
            capabilities: vec![Capability::Execute],
            sandbox: Sandbox::None,
            default_max_attempts: 1,
            deprecated: false,
            superseded_by: None,
        },
        NodeDefinition {
            key: "date_time",
            version: "1.0.0",
            label: "Date & time",
            description: "Reads the clock, or formats a timestamp, with no side effect.",
            category: NodeCategory::Helper,
            icon: "Clock4",
            docs_url: "https://docs.omnion.dev/nodes/date-time",
            inputs: vec![Port {
                name: "in".into(),
                kind: PortKind::Main,
                accepts: vec![],
                open: true,
            }],
            outputs: vec![Port {
                name: "out".into(),
                kind: PortKind::Main,
                accepts: vec!["json".into(), "text".into()],
                open: true,
            }],
            params: vec![ParamSpec {
                name: "operation".into(),
                kind: "string".into(),
                label: "Operation".into(),
                required: true,
                ui: ParamHint::Select,
                options: vec!["now".into(), "format".into(), "parse".into()],
                options_source: None,
                placeholder: None,
                help: Some("Formatting uses UTC unless a zone is given.".into()),
                default: Some(json!("now")),
                secret_field: false,
            }],
            credential_types: vec![],
            capabilities: vec![Capability::Execute],
            sandbox: Sandbox::None,
            default_max_attempts: 1,
            deprecated: false,
            superseded_by: None,
        },
        NodeDefinition {
            key: "stop_and_error",
            version: "1.0.0",
            label: "Stop and error",
            description: "Ends the run with a named error, so the error workflow is routed.",
            category: NodeCategory::ErrorHandler,
            icon: "OctagonAlert",
            docs_url: "https://docs.omnion.dev/nodes/stop-and-error",
            inputs: vec![Port {
                name: "in".into(),
                kind: PortKind::Main,
                accepts: vec![],
                open: true,
            }],
            outputs: vec![Port {
                name: "failed".into(),
                kind: PortKind::Error,
                accepts: vec!["json".into()],
                open: true,
            }],
            params: vec![ParamSpec {
                name: "message".into(),
                kind: "string".into(),
                label: "Message".into(),
                required: true,
                ui: ParamHint::Text,
                options: vec![],
                options_source: None,
                placeholder: Some("Rejected: the invoice total did not match.".into()),
                help: Some("The message is masked before it reaches a log line.".into()),
                default: None,
                secret_field: false,
            }],
            credential_types: vec![],
            capabilities: vec![Capability::Execute],
            sandbox: Sandbox::None,
            default_max_attempts: 1,
            deprecated: false,
            superseded_by: None,
        },
        NodeDefinition {
            key: "legacy_webhook",
            version: "0.9.0",
            label: "Webhook (legacy)",
            description: "The first inbound webhook node, kept for workflows that still name it.",
            category: NodeCategory::Integration,
            icon: "Webhook",
            docs_url: "https://docs.omnion.dev/nodes/http-request",
            inputs: vec![],
            outputs: vec![Port {
                name: "out".into(),
                kind: PortKind::Main,
                accepts: vec!["json".into(), "text".into()],
                open: true,
            }],
            params: vec![ParamSpec {
                name: "path".into(),
                kind: "string".into(),
                label: "Path".into(),
                required: true,
                ui: ParamHint::Text,
                options: vec![],
                options_source: None,
                placeholder: Some("orders".into()),
                help: None,
                default: None,
                secret_field: false,
            }],
            credential_types: vec![],
            capabilities: vec![Capability::Webhook, Capability::Trigger],
            sandbox: Sandbox::None,
            default_max_attempts: 1,
            deprecated: true,
            superseded_by: Some("http_request"),
        },
    ]
});

/// Every bundled node definition, in palette order.
#[must_use]
pub fn nodes() -> &'static [NodeDefinition] {
    &NODES
}

/// Every bundled credential type, in code.
static CREDENTIAL_TYPES: std::sync::LazyLock<Vec<CredentialDefinition>> =
    std::sync::LazyLock::new(|| {
        vec![
            CredentialDefinition {
                key: "api_key",
                kind: CredentialKind::ApiKey,
                label: "API key",
                description: "One opaque key sent as a header or a query parameter.",
                icon: "KeyRound",
                docs_url: "https://docs.omnion.dev/credentials/api-key",
                fields: vec![
                    CredentialField {
                        name: "api_key",
                        label: "API key",
                        kind: FieldType::Secret,
                        required: true,
                        options: vec![],
                        help: Some("Shown once, then never again.".into()),
                        never_log: true,
                    },
                    CredentialField {
                        name: "header",
                        label: "Header name",
                        kind: FieldType::String,
                        required: false,
                        options: vec![],
                        help: Some("Defaults to Authorization.".into()),
                        never_log: false,
                    },
                ],
                oauth: None,
                test_timeout_seconds: 10,
            },
            CredentialDefinition {
                key: "oauth2",
                kind: CredentialKind::OAuth2,
                label: "OAuth 2.0",
                description: "An authorization-code flow with a refreshable token set.",
                icon: "ShieldCheck",
                docs_url: "https://docs.omnion.dev/credentials/oauth2",
                fields: vec![
                    CredentialField {
                        name: "client_id",
                        label: "Client id",
                        kind: FieldType::String,
                        required: true,
                        options: vec![],
                        help: None,
                        never_log: false,
                    },
                    CredentialField {
                        name: "client_secret",
                        label: "Client secret",
                        kind: FieldType::Secret,
                        required: true,
                        options: vec![],
                        help: Some("Required for a confidential client only.".into()),
                        never_log: true,
                    },
                ],
                oauth: Some(OAuthConfig {
                    authorize_url: "https://auth.example.com/oauth/authorize",
                    token_url: "https://auth.example.com/oauth/token",
                    scopes: "read write",
                    pkce: true,
                    refresh: true,
                }),
                test_timeout_seconds: 15,
            },
            CredentialDefinition {
                key: "basic_auth",
                kind: CredentialKind::BasicAuth,
                label: "Basic auth",
                description: "A username and password pair.",
                icon: "UserRound",
                docs_url: "https://docs.omnion.dev/credentials/basic-auth",
                fields: vec![
                    CredentialField {
                        name: "username",
                        label: "Username",
                        kind: FieldType::String,
                        required: true,
                        options: vec![],
                        help: None,
                        never_log: false,
                    },
                    CredentialField {
                        name: "password",
                        label: "Password",
                        kind: FieldType::Secret,
                        required: true,
                        options: vec![],
                        help: None,
                        never_log: true,
                    },
                ],
                oauth: None,
                test_timeout_seconds: 10,
            },
            CredentialDefinition {
                key: "smtp",
                kind: CredentialKind::Smtp,
                label: "SMTP relay",
                description: "An outbound mail relay.",
                icon: "Mail",
                docs_url: "https://docs.omnion.dev/credentials/smtp",
                fields: vec![
                    CredentialField {
                        name: "host",
                        label: "Host",
                        kind: FieldType::String,
                        required: true,
                        options: vec![],
                        help: None,
                        never_log: false,
                    },
                    CredentialField {
                        name: "port",
                        label: "Port",
                        kind: FieldType::Number,
                        required: true,
                        options: vec![],
                        help: Some("465 for implicit TLS, 587 for STARTTLS.".into()),
                        never_log: false,
                    },
                    CredentialField {
                        name: "username",
                        label: "Username",
                        kind: FieldType::String,
                        required: true,
                        options: vec![],
                        help: None,
                        never_log: false,
                    },
                    CredentialField {
                        name: "password",
                        label: "Password",
                        kind: FieldType::Secret,
                        required: true,
                        options: vec![],
                        help: None,
                        never_log: true,
                    },
                ],
                oauth: None,
                test_timeout_seconds: 15,
            },
            CredentialDefinition {
                key: "cloud_storage",
                kind: CredentialKind::CloudStorage,
                label: "Object storage",
                description: "An S3-compatible endpoint with a key pair.",
                icon: "HardDrive",
                docs_url: "https://docs.omnion.dev/credentials/cloud-storage",
                fields: vec![
                    CredentialField {
                        name: "endpoint",
                        label: "Endpoint",
                        kind: FieldType::Url,
                        required: true,
                        options: vec![],
                        help: None,
                        never_log: false,
                    },
                    CredentialField {
                        name: "region",
                        label: "Region",
                        kind: FieldType::String,
                        required: true,
                        options: vec![],
                        help: None,
                        never_log: false,
                    },
                    CredentialField {
                        name: "access_key_id",
                        label: "Access key id",
                        kind: FieldType::String,
                        required: true,
                        options: vec![],
                        help: None,
                        never_log: false,
                    },
                    CredentialField {
                        name: "secret_access_key",
                        label: "Secret access key",
                        kind: FieldType::Secret,
                        required: true,
                        options: vec![],
                        help: None,
                        never_log: true,
                    },
                ],
                oauth: None,
                test_timeout_seconds: 15,
            },
        ]
    });

/// Every bundled credential type, in picker order.
#[must_use]
pub fn credential_types() -> &'static [CredentialDefinition] {
    &CREDENTIAL_TYPES
}

// ---------------------------------------------------------------------------------------------
// Lookup and lint
// ---------------------------------------------------------------------------------------------

/// Find one node by key.
#[must_use]
pub fn find_node(key: &str) -> Option<&'static NodeDefinition> {
    nodes().iter().find(|node| node.key == key)
}

/// Find one credential type by key.
#[must_use]
pub fn find_credential_type(key: &str) -> Option<&'static CredentialDefinition> {
    CREDENTIAL_TYPES
        .iter()
        .find(|definition| definition.key == key)
}

/// The bundled node keys, in palette order.
#[must_use]
pub fn node_keys() -> Vec<&'static str> {
    nodes().iter().map(|node| node.key).collect()
}

/// The bundled credential type keys, in picker order.
#[must_use]
pub fn credential_type_keys() -> Vec<&'static str> {
    credential_types()
        .iter()
        .map(|definition| definition.key)
        .collect()
}

/// Lint a node definition. Every finding is a refusal the registry refuses to install.
///
/// The rules are the ones that make a rendered form usable, and they are all one-directional —
/// a definition is rejected for a defect a person would otherwise discover as a broken screen.
pub fn lint_node(node: &NodeDefinition, credential_keys: &[&str]) -> Vec<LintFinding> {
    let mut findings = Vec::new();
    let mut push = |code: &'static str, message: String| {
        findings.push(LintFinding {
            code,
            subject: node.key.to_string(),
            message,
        });
    };

    if node.key.trim().is_empty() || node.key.contains(char::is_whitespace) {
        push(
            "node_key_invalid",
            format!(
                "a node key is lower-case and never contains a space: {:?}",
                node.key
            ),
        );
    }
    if node.label.trim().is_empty() {
        push("node_label_blank", "a node needs a label".into());
    }
    if node.version.trim().is_empty() {
        push("node_version_blank", "a node needs a version".into());
    }
    if node.icon.trim().is_empty() {
        push(
            "node_icon_missing",
            "a node needs a Lucide icon name, never a placeholder".into(),
        );
    }
    if node.docs_url.trim().is_empty() {
        push(
            "node_docs_missing",
            "a node needs a documentation link".into(),
        );
    }
    if node.capabilities.is_empty() {
        push(
            "node_capability_missing",
            "a node declares at least one capability".into(),
        );
    }
    if !(1..=crate::definition::MAX_ATTEMPTS).contains(&node.default_max_attempts) {
        push(
            "node_default_attempts_out_of_range",
            format!(
                "the default attempt count is {} and the engine takes 1 to {}",
                node.default_max_attempts,
                crate::definition::MAX_ATTEMPTS
            ),
        );
    }

    // Ports: unique names per direction, at least one main output, one open main output.
    lint_ports(&node.inputs, "input", &mut push);
    lint_ports(&node.outputs, "output", &mut push);

    // A node needs a main output unless it is a *terminal*: an error handler whose only open
    // port is the error port exists precisely to end the run and route the failure. Requiring a
    // main output there would force a port nothing can legitimately connect to.
    let main_outputs = node
        .outputs
        .iter()
        .filter(|port| port.kind == PortKind::Main)
        .count();
    let open_error = node
        .outputs
        .iter()
        .any(|port| port.kind == PortKind::Error && port.open);
    let is_terminal_error_handler =
        node.category == NodeCategory::ErrorHandler && main_outputs == 0 && open_error;
    if main_outputs == 0 && !is_terminal_error_handler {
        push(
            "node_no_main_output",
            "a node must have one main output, or nothing downstream can receive its items".into(),
        );
    }
    if node.is_trigger() && !node.inputs.is_empty() {
        push(
            "node_trigger_has_input",
            "a trigger starts a run, so it takes no input port".into(),
        );
    }

    // Parameters: unique names, renderable options, a credential field is a key not a secret.
    let mut seen: Vec<&str> = Vec::new();
    for param in &node.params {
        if param.name.trim().is_empty() {
            push("node_param_name_blank", "a parameter needs a name".into());
            continue;
        }
        if seen.contains(&param.name.as_str()) {
            push(
                "node_param_duplicate",
                format!("two parameters are both named \"{}\"", param.name),
            );
        }
        seen.push(&param.name);

        if param.label.trim().is_empty() {
            push(
                "node_param_label_blank",
                format!("parameter \"{}\" needs a label", param.name),
            );
        }
        if !matches!(
            param.kind.as_str(),
            "string" | "number" | "integer" | "boolean" | "object"
        ) {
            push(
                "node_param_type_unsupported",
                format!(
                    "parameter \"{}\" is typed {:?}; the palette renders string, number, integer, \
                     boolean and object",
                    param.name, param.kind
                ),
            );
        }
        if param.ui == ParamHint::Select
            && param.options.is_empty()
            && param.options_source.is_none()
        {
            push(
                "node_param_select_without_options",
                format!(
                    "parameter \"{}\" renders as a select but has neither an enum nor an \
                     options_source",
                    param.name
                ),
            );
        }
        if param.ui != ParamHint::Select && !param.options.is_empty() {
            push(
                "node_param_enum_without_select",
                format!(
                    "parameter \"{}\" carries an enum but renders as {:?}",
                    param.name, param.ui
                ),
            );
        }
        if param.ui == ParamHint::Number && param.kind != "number" && param.kind != "integer" {
            push(
                "node_param_number_type",
                format!(
                    "parameter \"{}\" renders as a number but is typed {:?}",
                    param.name, param.kind
                ),
            );
        }
        if param.ui == ParamHint::Boolean && param.kind != "boolean" {
            push(
                "node_param_boolean_type",
                format!(
                    "parameter \"{}\" renders as a checkbox but is typed {:?}",
                    param.name, param.kind
                ),
            );
        }
        if param.secret_field && param.ui != ParamHint::Select {
            push(
                "node_param_secret_field_shape",
                format!(
                    "parameter \"{}\" names a credential, so it renders as a select the palette \
                     fills from the credential list",
                    param.name
                ),
            );
        }
    }

    // Credential references resolve, and an OAuth type is only usable by a node that also
    // declares the capability its flow needs.
    for credential_key in &node.credential_types {
        if !credential_keys.contains(credential_key) {
            push(
                "node_credential_type_unknown",
                format!("no credential type is registered under \"{credential_key}\""),
            );
        }
    }
    let selects_credential = node
        .params
        .iter()
        .any(|param| param.secret_field && param.name == "credential_key");
    if !node.credential_types.is_empty() && !selects_credential {
        push(
            "node_credential_not_selectable",
            format!(
                "node \"{}\" accepts a credential but has no credential_key parameter for the \
                 palette to fill",
                node.key
            ),
        );
    }

    if node.deprecated && node.superseded_by.is_none() {
        push(
            "node_deprecated_without_replacement",
            "a deprecated node names the key that replaces it".into(),
        );
    }
    findings
}

/// Lint one node's ports: unique names, and an output port that is open or named.
fn lint_ports(ports: &[Port], direction: &str, push: &mut impl FnMut(&'static str, String)) {
    let mut seen: Vec<&str> = Vec::new();
    for port in ports {
        if port.name.trim().is_empty() {
            push(
                "node_port_name_blank",
                format!("a {direction} port needs a name"),
            );
            continue;
        }
        if seen.contains(&port.name.as_str()) {
            push(
                "node_port_duplicate",
                format!("two {direction} ports are both named \"{}\"", port.name),
            );
        }
        seen.push(&port.name);
    }
}

/// Lint a credential definition.
pub fn lint_credential(definition: &CredentialDefinition, used_by: &[&str]) -> Vec<LintFinding> {
    let mut findings = Vec::new();
    let mut push = |code: &'static str, message: String| {
        findings.push(LintFinding {
            code,
            subject: definition.key.to_string(),
            message,
        });
    };

    if definition.key.trim().is_empty() {
        push(
            "credential_key_invalid",
            "a credential type needs a key".into(),
        );
    }
    if definition.label.trim().is_empty() {
        push(
            "credential_label_blank",
            "a credential type needs a label".into(),
        );
    }
    if definition.icon.trim().is_empty() {
        push(
            "credential_icon_missing",
            "a credential type needs a Lucide icon name".into(),
        );
    }
    if definition.fields.is_empty() {
        push(
            "credential_no_fields",
            "a credential type with no fields is a form nobody can fill in".into(),
        );
    }
    if definition.test_timeout_seconds <= 0 || definition.test_timeout_seconds > 120 {
        push(
            "credential_test_timeout_out_of_range",
            format!(
                "the test hook timeout is {}s and the range is 1 to 120",
                definition.test_timeout_seconds
            ),
        );
    }
    if definition.kind == CredentialKind::OAuth2 && definition.oauth.is_none() {
        push(
            "credential_oauth_config_missing",
            "an oauth2 type declares its authorize and token endpoints".into(),
        );
    }
    if definition.kind != CredentialKind::OAuth2 && definition.oauth.is_some() {
        push(
            "credential_oauth_config_unexpected",
            format!(
                "a {} type has no authorization-code flow, so an oauth config on it is a bug",
                definition.kind.as_str()
            ),
        );
    }
    if let Some(oauth) = &definition.oauth {
        if oauth.authorize_url.trim().is_empty() || oauth.token_url.trim().is_empty() {
            push(
                "credential_oauth_url_blank",
                "an oauth config names both endpoints".into(),
            );
        }
        if oauth.refresh && oauth.scopes.trim().is_empty() {
            push(
                "credential_oauth_scopes_blank",
                "a refreshable token set requests at least one scope".into(),
            );
        }
    }

    let mut seen: Vec<&str> = Vec::new();
    for field in &definition.fields {
        if field.name.trim().is_empty() {
            push("credential_field_name_blank", "a field needs a name".into());
            continue;
        }
        if seen.contains(&field.name) {
            push(
                "credential_field_duplicate",
                format!("two fields are both named \"{}\"", field.name),
            );
        }
        seen.push(field.name);
        if field.label.trim().is_empty() {
            push(
                "credential_field_label_blank",
                format!("field \"{}\" needs a label", field.name),
            );
        }
        if field.kind == FieldType::Secret && !field.never_log {
            push(
                "credential_secret_field_logged",
                format!(
                    "field \"{}\" is a secret, so never_log must be true; a logged secret is not \
                     a secret",
                    field.name
                ),
            );
        }
        if field.kind == FieldType::Select && field.options.is_empty() {
            push(
                "credential_field_select_without_options",
                format!("field \"{}\" is a select with no options", field.name),
            );
        }
    }

    if used_by.is_empty() {
        push(
            "credential_type_orphaned",
            format!(
                "no node accepts the \"{}\" type, so the form has nothing to fill it for",
                definition.key
            ),
        );
    }

    findings
}

/// Lint the whole registry: duplicate keys, node rules, credential rules and the cross
/// references between them.
///
/// A node naming a credential type that does not exist is reported on the node; a credential
/// type no node accepts is reported on the type. Both are failures, because both are a screen
/// that cannot be rendered.
#[must_use]
pub fn lint() -> Vec<LintFinding> {
    let mut findings = Vec::new();

    // Duplicate node keys: a map would hide one, so the table is scanned pairwise.
    for (index, node) in nodes().iter().enumerate() {
        let earlier = nodes()[..index]
            .iter()
            .find(|other| other.key == node.key)
            .map(|other| other.version);
        if let Some(version) = earlier {
            findings.push(LintFinding {
                code: "node_duplicate_key",
                subject: node.key.to_string(),
                message: format!(
                    "the key is registered twice, with {version} and {}",
                    node.version
                ),
            });
        }
    }
    for (index, definition) in credential_types().iter().enumerate() {
        let earlier = credential_types()[..index]
            .iter()
            .find(|other| other.key == definition.key);
        if earlier.is_some() {
            findings.push(LintFinding {
                code: "credential_type_duplicate_key",
                subject: definition.key.to_string(),
                message: "the credential type key is registered twice".into(),
            });
        }
    }

    let credential_keys = credential_type_keys();
    for node in nodes() {
        findings.extend(lint_node(node, &credential_keys));
    }

    for definition in credential_types() {
        let used_by: Vec<&str> = nodes()
            .iter()
            .filter(|node| {
                node.credential_types
                    .iter()
                    .any(|key| *key == definition.key)
            })
            .map(|node| node.key)
            .collect();
        findings.extend(lint_credential(definition, &used_by));
    }

    // A supersession pointer must resolve. Checked here rather than inside `lint_node` so a
    // node in isolation can still be linted (the node-package path of REQ-087 slice 4 does).
    for node in nodes() {
        if let Some(replacement) = node.superseded_by {
            if find_node(replacement).is_none() {
                findings.push(LintFinding {
                    code: "node_superseded_by_unknown",
                    subject: node.key.to_string(),
                    message: format!("\"{replacement}\" is not in the registry"),
                });
            }
        }
    }

    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundled_registry_lints_clean() {
        let findings = lint();
        assert!(
            findings.is_empty(),
            "the shipped registry must lint clean, got: {findings:#?}"
        );
    }

    #[test]
    fn every_category_holds_at_least_one_node() {
        for category in NodeCategory::all() {
            assert!(
                nodes().iter().any(|node| node.category == category),
                "the palette has an empty category: {:?}",
                category.as_str()
            );
        }
    }

    #[test]
    fn a_deprecated_node_names_a_real_replacement() {
        let legacy = find_node("legacy_webhook").expect("the deprecated node is registered");
        assert!(legacy.deprecated);
        let replacement = legacy.superseded_by.expect("a replacement is named");
        let target = find_node(replacement).expect("the replacement is registered");
        assert!(!target.deprecated, "a replacement is not itself deprecated");
    }

    #[test]
    fn a_credential_type_no_node_accepts_is_a_finding() {
        let definition = CredentialDefinition {
            key: "ssh_key",
            kind: CredentialKind::SshKey,
            label: "SSH key".into(),
            description: "d".into(),
            icon: "Key".into(),
            docs_url: "https://docs.omnion.dev".into(),
            fields: vec![CredentialField {
                name: "private_key",
                label: "Private key".into(),
                kind: FieldType::Secret,
                required: true,
                options: vec![],
                help: None,
                never_log: true,
            }],
            oauth: None,
            test_timeout_seconds: 10,
        };
        let findings = lint_credential(&definition, &[]);
        assert!(
            findings
                .iter()
                .any(|f| f.code == "credential_type_orphaned"),
            "an orphan type is refused, got: {findings:#?}"
        );
        assert!(lint_credential(&definition, &["some_node"]).is_empty());
    }

    #[test]
    fn a_logged_secret_field_is_a_finding() {
        let definition = CredentialDefinition {
            key: "api_key",
            kind: CredentialKind::ApiKey,
            label: "API key".into(),
            description: "d".into(),
            icon: "Key".into(),
            docs_url: "https://docs.omnion.dev".into(),
            fields: vec![CredentialField {
                name: "api_key",
                label: "API key".into(),
                kind: FieldType::Secret,
                required: true,
                options: vec![],
                help: None,
                never_log: false,
            }],
            oauth: None,
            test_timeout_seconds: 10,
        };
        let findings = lint_credential(&definition, &["http_request"]);
        assert!(
            findings
                .iter()
                .any(|f| f.code == "credential_secret_field_logged"),
            "a secret field that is logged is refused, got: {findings:#?}"
        );
    }

    #[test]
    fn an_error_output_cannot_feed_a_main_input() {
        let source = find_node("if").expect("if is registered");
        let target = find_node("http_request").expect("http is registered");

        // A node whose error port is open and whose main output is closed.
        let router = NodeDefinition {
            key: "router".into(),
            version: "1.0.0".into(),
            label: "Router".into(),
            description: "d".into(),
            category: NodeCategory::Flow,
            icon: "GitBranch".into(),
            docs_url: "https://docs.omnion.dev".into(),
            inputs: vec![Port {
                name: "in".into(),
                kind: PortKind::Main,
                accepts: vec![],
                open: true,
            }],
            outputs: vec![Port {
                name: "failed".into(),
                kind: PortKind::Error,
                accepts: vec!["json".into()],
                open: true,
            }],
            params: vec![],
            credential_types: vec![],
            capabilities: vec![Capability::Execute],
            sandbox: Sandbox::None,
            default_max_attempts: 1,
            deprecated: false,
            superseded_by: None,
        };

        let error = router
            .check_connection("failed", target, "in")
            .expect_err("an error output cannot reach a main input");
        assert_eq!(error.code(), "connection_type_mismatch");
        // `if` does have a main input, so this refusal is the *kind*, not a missing port.
        assert_eq!(
            router
                .check_connection("failed", source, "in")
                .expect_err("an error output cannot reach a main input")
                .code(),
            "connection_type_mismatch"
        );

        // A port that does not exist is a different refusal, and the message must name it.
        assert_eq!(
            router
                .check_connection("failed", source, "no_such_port")
                .expect_err("an input that does not exist is refused")
                .code(),
            "connection_port_unknown"
        );

        // A node that does declare an error input accepts the error output.
        let handler = NodeDefinition {
            key: "handler".into(),
            version: "1.0.0".into(),
            label: "Handler".into(),
            description: "d".into(),
            category: NodeCategory::ErrorHandler,
            icon: "ShieldCheck".into(),
            docs_url: "https://docs.omnion.dev".into(),
            inputs: vec![Port {
                name: "failed".into(),
                kind: PortKind::Error,
                accepts: vec!["json".into()],
                open: true,
            }],
            outputs: vec![Port {
                name: "out".into(),
                kind: PortKind::Main,
                accepts: vec!["json".into()],
                open: true,
            }],
            params: vec![],
            credential_types: vec![],
            capabilities: vec![Capability::Execute],
            sandbox: Sandbox::None,
            default_max_attempts: 1,
            deprecated: false,
            superseded_by: None,
        };
        router
            .check_connection("failed", &handler, "failed")
            .expect("an error output reaches a declared error input");
    }

    #[test]
    fn an_unknown_port_is_named() {
        let source = find_node("if").expect("if is registered");
        let target = find_node("http_request").expect("http is registered");
        let error = source
            .check_connection("nope", target, "in")
            .expect_err("an unknown output is refused");
        assert_eq!(error.code(), "connection_port_unknown");
        assert!(error.to_string().contains("nope"));
    }

    #[test]
    fn a_select_without_options_is_a_finding() {
        let mut node = find_node("http_request").expect("registered").clone();
        node.params[0].ui = ParamHint::Select;
        let findings = lint_node(&node, &credential_type_keys());
        assert!(
            findings
                .iter()
                .any(|f| f.code == "node_param_select_without_options"),
            "a select nobody can fill is refused, got: {findings:#?}"
        );
    }

    #[test]
    fn a_node_with_two_open_main_outputs_is_a_finding() {
        let mut node = find_node("if").expect("registered").clone();
        node.outputs[1].open = false;
        let clean = lint_node(&node, &credential_type_keys());
        assert!(
            !clean.iter().any(|f| f.code == "node_no_main_output"),
            "one open main output is enough: {clean:#?}"
        );
        node.outputs = node.outputs[..1].to_vec();
        assert!(
            !lint_node(&node, &credential_type_keys())
                .iter()
                .any(|f| f.code == "node_no_main_output"),
            "one main output is enough"
        );

        // Strip the main output entirely: a flow node with no main output is refused.
        node.outputs = node.outputs[1..].to_vec();
        let findings = lint_node(&node, &credential_type_keys());
        assert!(
            findings.iter().any(|f| f.code == "node_no_main_output"),
            "a flow node with no main output is refused, got: {findings:#?}"
        );

        // The same shape in the error-handler category is a terminal and is allowed, because
        // the only reason to have one is to end the run and route the failure. The bundled
        // `stop_and_error` node is that shape, so the assertion uses it rather than a hand-built
        // clone that could drift from the rule it is proving.
        let terminal = find_node("stop_and_error").expect("the terminal node is registered");
        assert!(
            !lint_node(terminal, &credential_type_keys())
                .iter()
                .any(|f| f.code == "node_no_main_output"),
            "a terminal error handler needs no main output"
        );
    }

    #[test]
    fn a_credential_selecting_node_without_the_parameter_is_a_finding() {
        let mut node = find_node("http_request").expect("registered").clone();
        node.params.retain(|param| !param.secret_field);
        let findings = lint_node(&node, &credential_type_keys());
        assert!(
            findings
                .iter()
                .any(|f| f.code == "node_credential_not_selectable"),
            "a credential nobody can pick is refused, got: {findings:#?}"
        );
    }

    #[test]
    fn a_trigger_with_an_input_is_a_finding() {
        let mut node = find_node("manual_trigger").expect("registered").clone();
        node.inputs.push(Port {
            name: "in".into(),
            kind: PortKind::Main,
            accepts: vec![],
            open: true,
        });
        let findings = lint_node(&node, &credential_type_keys());
        assert!(
            findings.iter().any(|f| f.code == "node_trigger_has_input"),
            "a trigger with an input is refused, got: {findings:#?}"
        );
    }
}
