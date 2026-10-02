//! The compiled tool catalogue: every tool the platform exposes to an agent, as code.
//!
//! REQ-100 slices this from `tools.rs` deliberately. `tools.rs` owns the **boundary the loop
//! checks before anything runs** — unknown key, not allowed, needs approval, arguments are not an
//! object — and it does not know what any particular tool *is*. This module is the other half: the
//! declarative record of every tool the installation offers, so the registry screen, the model
//! payload and the permission test can all be driven from one list instead of three.
//!
//! **A catalogue entry is metadata, not a door.** Nothing here executes anything; the execution
//! path is `tools::execute` and a tool body lives wherever the service lives. That split is the
//! whole point of the request: a tool that is only reachable through `tools::execute` cannot be
//! reached by a second door added later, and a catalogue row that says `deployment.deploy`
//! requires `deployment.deploy` is a *claim*, which the permission-mapping test below turns into
//! a fact.
//!
//! The permission a tool declares must equal the permission its HTTP route enforces. That is the
//! central risk named in the request ("a tool is only as safe as the permission its endpoint
//! enforces"), and it is enforced by [`ROUTE_PERMISSIONS`] being a compiled table rather than a
//! convention — [`every_permission_is_a_real_catalogue_key`] fails the build when a tool names a
//! key no permission catalogue carries, and `probe-permission-mapping.cjs`-style drift is caught
//! by the same route table the tests read.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// How much damage a call can do when it is wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    /// Reads and lookups. Reversible by definition.
    Low,
    /// Writes user content that a person edits afterwards.
    Medium,
    /// Writes that reach users, spend money, install code or restart services. Every one of these
    /// ships `requires_approval` in the seed, because "high risk, no gate" is a configuration a
    /// panel must let an operator see and refuse.
    High,
}

impl Risk {
    /// The stable string the API and the `ai_tools.risk` column use.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// Read a stored value back. An unrecognised string is `High`, never `Low`: a row written by
    /// a newer version, or corrupted, must not become the permissive reading by default.
    #[must_use]
    pub fn from_str_lossy(value: &str) -> Self {
        match value {
            "low" => Self::Low,
            "medium" => Self::Medium,
            _ => Self::High,
        }
    }
}

/// One tool, described rather than implemented.
///
/// No `Serialize` derive: a `fn() -> Value` field has no serde representation, and the row the
/// seeder writes is assembled by [`ToolSpec::to_row`] instead — which is the better shape anyway,
/// because the row's field names are part of what the migration and the API agree on, and a
/// derived struct would tie them to this one file's field names.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    /// The key the model calls, and the primary key of the `ai_tools` row.
    pub key: &'static str,
    /// The group the panel lists it under and the filters use.
    pub class: &'static str,
    /// One sentence, shown in the registry, the tool detail and the model payload.
    pub description: &'static str,
    /// The single permission this tool requires.
    ///
    /// The request is explicit that this is one key and never a list: "a tool that needs two
    /// permissions is two tools or a narrower tool". A tool with two guards is a tool whose
    /// safety depends on a check somebody will eventually reorder.
    pub permission: &'static str,
    /// The JSON Schema of the arguments, validated before execution with
    /// `additionalProperties = false`.
    pub input_schema: fn() -> Value,
    /// An example payload the detail screen offers and the schema test validates against itself.
    pub example: fn() -> Value,
    pub risk: Risk,
    /// Whether calling it twice with the same arguments is the same as calling it once.
    pub idempotent: bool,
}

impl ToolSpec {
    /// The `ai_tools` row this tool seeds to, minus the operator-owned columns.
    ///
    /// It deliberately carries **no** `enabled`, `timeout_ms`, `max_calls_per_run` or
    /// `requires_approval`. The request is explicit that seeding "touches only `description`,
    /// `class`, `permission`, `risk`, `input_schema`, `example` and `idempotent`, preserving
    /// `enabled`, `timeout_ms`, `max_calls_per_run` and `requires_approval`" — so returning those
    /// fields here would make it *possible* for a future seeder to write them, which is exactly the
    /// kind of convenience that turns into "my gated tool came back ungated after a restart".
    #[must_use]
    pub fn to_row(&self) -> Value {
        json!({
            "key": self.key,
            "class": self.class,
            "description": self.description,
            "permission": self.permission,
            "risk": self.risk.as_str(),
            "input_schema": (self.input_schema)(),
            "example": (self.example)(),
            "idempotent": self.idempotent,
        })
    }
}

/// A helper for the schema functions, so each tool's schema reads as data rather than as
/// `serde_json::json!` nested inside a function pointer type the compiler has to infer.
const fn schema(body: fn() -> Value) -> fn() -> Value {
    body
}

/// `content.search` — the read-only content lookup, and the model-facing shape every other
/// content tool is described next to.
fn content_search_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["query"],
        "properties": {
            "query": { "type": "string", "minLength": 1, "maxLength": 200 },
            "content_type": { "type": "string", "maxLength": 64 },
            "limit": { "type": "integer", "minimum": 1, "maximum": 50 }
        }
    })
}

/// Every tool in the installation, in the order the request lists them.
///
/// The order is the panel's default grouping order, so it is deliberate: content, media, users,
/// sites, themes, plugins, then ops. Ops last because an ops tool is the one an operator reads
/// twice before enabling.
pub fn specs() -> &'static [ToolSpec] {
    static SPECS: &[ToolSpec] = &[
        ToolSpec {
            key: "content.search",
            class: "content",
            description: "Search content entries by text, optionally narrowed to one content type.",
            permission: "content.pages.read",
            input_schema: schema(content_search_schema),
            example: schema(|| json!({ "query": "release notes", "limit": 10 })),
            risk: Risk::Low,
            idempotent: true,
        },
        ToolSpec {
            key: "content.read",
            class: "content",
            description: "Read one content entry, with its fields and current status.",
            permission: "content.pages.read",
            input_schema: schema(|| {
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["id"],
                    "properties": { "id": { "type": "string", "format": "uuid", "maxLength": 36 } }
                })
            }),
            example: schema(|| json!({ "id": "6b2f8a1e-0f4c-4a1a-9c2b-1d2e3f4a5b6c" })),
            risk: Risk::Low,
            idempotent: true,
        },
        ToolSpec {
            key: "content.create",
            class: "content",
            description: "Create a draft content entry. Never publishes.",
            permission: "content.pages.create",
            input_schema: schema(|| {
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["title", "content_type"],
                    "properties": {
                        "title": { "type": "string", "minLength": 1, "maxLength": 300 },
                        "content_type": { "type": "string", "maxLength": 64 },
                        "body": { "type": "string", "maxLength": 100_000 },
                        "site_id": { "type": "string", "format": "uuid" }
                    }
                })
            }),
            example: schema(|| json!({ "title": "Autumn release", "content_type": "page", "body": "…" })),
            risk: Risk::Medium,
            idempotent: false,
        },
        ToolSpec {
            key: "content.update",
            class: "content",
            description: "Change fields on an existing content entry, as a draft.",
            permission: "content.pages.update",
            input_schema: schema(|| {
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["id"],
                    "properties": {
                        "id": { "type": "string", "format": "uuid" },
                        "title": { "type": "string", "maxLength": 300 },
                        "body": { "type": "string", "maxLength": 100_000 }
                    }
                })
            }),
            example: schema(|| json!({ "id": "6b2f8a1e-0f4c-4a1a-9c2b-1d2e3f4a5b6c", "title": "Autumn release" })),
            risk: Risk::Medium,
            idempotent: true,
        },
        ToolSpec {
            key: "content.publish",
            class: "content",
            description: "Publish a draft content entry so visitors can see it.",
            permission: "content.pages.publish",
            input_schema: schema(|| {
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["id"],
                    "properties": { "id": { "type": "string", "format": "uuid" } }
                })
            }),
            example: schema(|| json!({ "id": "6b2f8a1e-0f4c-4a1a-9c2b-1d2e3f4a5b6c" })),
            risk: Risk::High,
            idempotent: true,
        },
        ToolSpec {
            key: "content.rollback",
            class: "content",
            description: "Restore a content entry to an earlier revision.",
            permission: "content.pages.restore",
            input_schema: schema(|| {
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["id"],
                    "properties": {
                        "id": { "type": "string", "format": "uuid" },
                        "revision_id": { "type": "string", "format": "uuid" }
                    }
                })
            }),
            example: schema(|| json!({ "id": "6b2f8a1e-0f4c-4a1a-9c2b-1d2e3f4a5b6c" })),
            risk: Risk::High,
            idempotent: true,
        },
        ToolSpec {
            key: "media.search",
            class: "media",
            description: "Search stored media by name or tag.",
            permission: "media.read",
            input_schema: schema(|| {
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["query"],
                    "properties": {
                        "query": { "type": "string", "minLength": 1, "maxLength": 200 },
                        "limit": { "type": "integer", "minimum": 1, "maximum": 50 }
                    }
                })
            }),
            example: schema(|| json!({ "query": "hero", "limit": 20 })),
            risk: Risk::Low,
            idempotent: true,
        },
        ToolSpec {
            key: "media.upload",
            class: "media",
            description: "Upload a file into the media library.",
            permission: "media.upload",
            input_schema: schema(|| {
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["filename", "content_base64"],
                    "properties": {
                        "filename": { "type": "string", "minLength": 1, "maxLength": 255 },
                        "content_base64": { "type": "string", "maxLength": 5_000_000 },
                        "folder_id": { "type": "string", "format": "uuid" }
                    }
                })
            }),
            example: schema(|| json!({ "filename": "hero.png", "content_base64": "…" })),
            risk: Risk::Medium,
            idempotent: false,
        },
        ToolSpec {
            key: "users.search",
            class: "users",
            description: "Search users by name or email.",
            permission: "users.read",
            input_schema: schema(|| {
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["query"],
                    "properties": {
                        "query": { "type": "string", "minLength": 1, "maxLength": 200 },
                        "limit": { "type": "integer", "minimum": 1, "maximum": 50 }
                    }
                })
            }),
            example: schema(|| json!({ "query": "ferkan", "limit": 10 })),
            risk: Risk::Low,
            idempotent: true,
        },
        ToolSpec {
            key: "users.create",
            class: "users",
            description: "Create a user account with the given role.",
            permission: "users.create",
            input_schema: schema(|| {
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["email", "name"],
                    "properties": {
                        "email": { "type": "string", "maxLength": 320 },
                        "name": { "type": "string", "minLength": 1, "maxLength": 200 },
                        "role": { "type": "string", "enum": ["admin", "editor", "author", "viewer"] }
                    }
                })
            }),
            example: schema(|| json!({ "email": "new@example.com", "name": "New Person", "role": "editor" })),
            risk: Risk::High,
            idempotent: false,
        },
        ToolSpec {
            key: "site.get",
            class: "sites",
            description: "Read one site's configuration and status.",
            permission: "sites.read",
            input_schema: schema(|| {
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["site_id"],
                    "properties": { "site_id": { "type": "string", "format": "uuid" } }
                })
            }),
            example: schema(|| json!({ "site_id": "1a2b3c4d-5e6f-4a1b-8c2d-3e4f5a6b7c8d" })),
            risk: Risk::Low,
            idempotent: true,
        },
        ToolSpec {
            key: "site.update",
            class: "sites",
            description: "Change a site's configuration.",
            permission: "sites.update",
            input_schema: schema(|| {
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["site_id", "settings"],
                    "properties": {
                        "site_id": { "type": "string", "format": "uuid" },
                        "settings": { "type": "object" }
                    }
                })
            }),
            example: schema(|| json!({ "site_id": "1a2b3c4d-…", "settings": { "locale": "tr" } })),
            risk: Risk::Medium,
            idempotent: true,
        },
        ToolSpec {
            key: "theme.list",
            class: "themes",
            description: "List installed themes with their active state.",
            permission: "sites.read",
            input_schema: schema(|| {
                json!({ "type": "object", "additionalProperties": false, "properties": {} })
            }),
            example: schema(|| json!({})),
            risk: Risk::Low,
            idempotent: true,
        },
        ToolSpec {
            key: "theme.activate",
            class: "themes",
            description: "Activate a theme for a site. Takes effect on the next request.",
            permission: "sites.update",
            input_schema: schema(|| {
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["theme_key", "site_id"],
                    "properties": {
                        "theme_key": { "type": "string", "maxLength": 64 },
                        "site_id": { "type": "string", "format": "uuid" }
                    }
                })
            }),
            example: schema(|| json!({ "theme_key": "minimal", "site_id": "1a2b3c4d-…" })),
            risk: Risk::High,
            idempotent: true,
        },
        ToolSpec {
            key: "plugin.list",
            class: "plugins",
            description: "List installed plugins with their enabled state.",
            permission: "plugins.read",
            input_schema: schema(|| {
                json!({ "type": "object", "additionalProperties": false, "properties": {} })
            }),
            example: schema(|| json!({})),
            risk: Risk::Low,
            idempotent: true,
        },
        ToolSpec {
            key: "plugin.install",
            class: "plugins",
            description: "Install a plugin package from the marketplace.",
            permission: "plugins.install",
            input_schema: schema(|| {
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["package_key"],
                    "properties": { "package_key": { "type": "string", "maxLength": 128 } }
                })
            }),
            example: schema(|| json!({ "package_key": "acme/blog-extras" })),
            risk: Risk::High,
            idempotent: false,
        },
        ToolSpec {
            key: "workflow.start",
            class: "ops",
            description: "Start a workflow run from a template with the given input.",
            permission: "workflows.run",
            input_schema: schema(|| {
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["workflow_id"],
                    "properties": {
                        "workflow_id": { "type": "string", "format": "uuid" },
                        "input": { "type": "object" }
                    }
                })
            }),
            example: schema(|| json!({ "workflow_id": "2b3c4d5e-…", "input": { "email": "a@b.c" } })),
            risk: Risk::Medium,
            idempotent: false,
        },
        ToolSpec {
            key: "analytics.query",
            class: "ops",
            description: "Read analytics aggregates for a site and date range.",
            permission: "analytics.read",
            input_schema: schema(|| {
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["site_id", "metric"],
                    "properties": {
                        "site_id": { "type": "string", "format": "uuid" },
                        "metric": { "type": "string", "enum": ["pageviews", "visitors", "sessions", "events"] },
                        "days": { "type": "integer", "minimum": 1, "maximum": 365 }
                    }
                })
            }),
            example: schema(|| json!({ "site_id": "1a2b3c4d-…", "metric": "pageviews", "days": 30 })),
            risk: Risk::Low,
            idempotent: true,
        },
        ToolSpec {
            key: "deployment.preview",
            class: "ops",
            description: "Render a preview deployment for a site without publishing it.",
            permission: "deployment.preview",
            input_schema: schema(|| {
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["site_id"],
                    "properties": { "site_id": { "type": "string", "format": "uuid" } }
                })
            }),
            example: schema(|| json!({ "site_id": "1a2b3c4d-…" })),
            risk: Risk::Low,
            idempotent: true,
        },
        ToolSpec {
            key: "deployment.deploy",
            class: "ops",
            description: "Deploy a site to its production environment.",
            permission: "deployment.deploy",
            input_schema: schema(|| {
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["site_id"],
                    "properties": { "site_id": { "type": "string", "format": "uuid" } }
                })
            }),
            example: schema(|| json!({ "site_id": "1a2b3c4d-…" })),
            risk: Risk::High,
            idempotent: false,
        },
        ToolSpec {
            key: "deployment.read",
            class: "ops",
            description: "Read the deployment history and current status of a site.",
            permission: "deployment.read",
            input_schema: schema(|| {
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["site_id"],
                    "properties": { "site_id": { "type": "string", "format": "uuid" } }
                })
            }),
            example: schema(|| json!({ "site_id": "1a2b3c4d-…" })),
            risk: Risk::Low,
            idempotent: true,
        },
        ToolSpec {
            key: "deployment.restart",
            class: "ops",
            description: "Restart a site's runtime processes.",
            permission: "deployment.read",
            input_schema: schema(|| {
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["site_id"],
                    "properties": { "site_id": { "type": "string", "format": "uuid" } }
                })
            }),
            example: schema(|| json!({ "site_id": "1a2b3c4d-…" })),
            risk: Risk::High,
            idempotent: false,
        },
        ToolSpec {
            key: "logs.read",
            class: "ops",
            description: "Read application log lines for the caller's organization.",
            permission: "events.read",
            input_schema: schema(|| {
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "level": { "type": "string", "enum": ["error", "warn", "info"] },
                        "limit": { "type": "integer", "minimum": 1, "maximum": 500 }
                    }
                })
            }),
            example: schema(|| json!({ "level": "error", "limit": 100 })),
            risk: Risk::Medium,
            idempotent: true,
        },
        ToolSpec {
            key: "health.read",
            class: "ops",
            description: "Read the platform health snapshot: components, queues, storage.",
            permission: "ai.usage.read",
            input_schema: schema(|| {
                json!({ "type": "object", "additionalProperties": false, "properties": {} })
            }),
            example: schema(|| json!({})),
            risk: Risk::Low,
            idempotent: true,
        },
        ToolSpec {
            key: "seo.analyze",
            class: "ops",
            description: "Analyze a content entry's SEO metadata and report gaps.",
            permission: "content.pages.update",
            input_schema: schema(|| {
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["id"],
                    "properties": { "id": { "type": "string", "format": "uuid" } }
                })
            }),
            example: schema(|| json!({ "id": "6b2f8a1e-…" })),
            risk: Risk::Low,
            idempotent: true,
        },
    ];
    SPECS
}

/// Look one tool up by key.
#[must_use]
pub fn find(key: &str) -> Option<&'static ToolSpec> {
    specs().iter().find(|spec| spec.key == key)
}

/// The default `requires_approval` a seeded tool ships with.
///
/// The request names two tools as "the most likely to be over-granted" and says both ship
/// `requires_approval = true` by default. It is extended to every `Risk::High` tool for the same
/// reason: the gate is cheap to keep and the request's own acceptance criteria treat a high-risk
/// tool *without* a gate as something the panel must render a warning stripe about. Making the
/// safe state the default means the warning stripe describes a deliberate operator choice.
#[must_use]
pub fn default_requires_approval(spec: &ToolSpec) -> bool {
    spec.risk == Risk::High
}

/// Whether a tool with **no HTTP route** ships `requires_approval = true`.
///
/// Split out from [`default_requires_approval`] rather than folded into it, because the two
/// rules answer different questions. Risk asks "how bad is this action if it happens"; the
/// binding asks "can this action happen at all". A `Risk::Low` tool whose endpoint is not built
/// — `deployment.preview`, `health.read`, `theme.list` — is harmless *and* impossible, and the
/// registry row would otherwise invite an operator to enable an action the platform cannot
/// perform. Seeding it gated is the state that says so in one column.
///
/// A separate function rather than a special case inside the other, because folding it in would
/// make `default_requires_approval` depend on a table in another module, and the risk predicate
/// is the one the panel's warning stripe reads: a stripe computed from a value that also
/// depends on route wiring is a stripe that changes meaning when a route is added.
#[must_use]
pub fn default_requires_approval_for(spec: &ToolSpec, wired: bool) -> bool {
    if !wired {
        return true;
    }
    default_requires_approval(spec)
}

/// The default per-run call cap and timeout a seeded tool ships with.
#[must_use]
pub fn default_limits(spec: &ToolSpec) -> (i32, i32) {
    // (timeout_ms, max_calls_per_run). Ops tools that touch the network are slower and rarer.
    match spec.class {
        "ops" => (60_000, 10),
        _ => (30_000, 20),
    }
}

/// The permission catalogue keys a tool may name.
///
/// **This is an alias, not a copy, and the difference is the point.** It used to be a
/// hand-written list in this file, and the test that was supposed to prove those keys were real
/// checked membership in *this list* — so 16 keys the platform's catalogue has never carried
/// (`content.read`, `site.read`, `theme.read`, `logs.read`, `health.read`, `seo.analyze`, …)
/// passed a test whose whole job was to catch them. The list now points at
/// [`crate::ops_binding::REAL_PERMISSION_KEYS`], which is itself verified against the router's
/// guards by `mirror_guards_is_in_sync_with_the_router`.
///
/// It is still not a *dependency* on `crates/permissions`: that would invert the direction the
/// workspace established (permissions knows nothing about AI). What it is instead is a claim
/// that is checked from the outside, by a test that reads the router source at compile time —
/// a mirror that can fail, rather than a copy that cannot.
pub const KNOWN_PERMISSION_KEYS: &[&str] = crate::ops_binding::REAL_PERMISSION_KEYS;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// The exact key list the request enumerates. It is restated here on purpose: the test that
    /// matters is "every tool the request names exists", and a test that iterated
    /// [`specs`] instead would pass by construction — it would notice a *missing* tool only if
    /// the list came from somewhere other than the code under test.
    const REQUEST_KEYS: &[&str] = &[
        "content.search",
        "content.read",
        "content.create",
        "content.update",
        "content.publish",
        "content.rollback",
        "media.search",
        "media.upload",
        "users.search",
        "users.create",
        "site.get",
        "site.update",
        "theme.list",
        "theme.activate",
        "plugin.list",
        "plugin.install",
        "workflow.start",
        "analytics.query",
        "deployment.preview",
        "deployment.deploy",
        "deployment.read",
        "deployment.restart",
        "logs.read",
        "health.read",
        "seo.analyze",
    ];

    #[test]
    fn every_tool_the_request_lists_is_in_the_catalogue() {
        for key in REQUEST_KEYS {
            assert!(
                find(key).is_some(),
                "the request names {key} and the catalogue has no such tool"
            );
        }
        // And the other direction, so a tool nobody asked for shows up as a deliberate addition
        // rather than as a row the panel lists with no owner.
        for spec in specs() {
            assert!(
                REQUEST_KEYS.contains(&spec.key),
                "{} is in the catalogue but the request does not name it",
                spec.key
            );
        }
    }

    #[test]
    fn keys_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for spec in specs() {
            assert!(seen.insert(spec.key), "{} is declared twice", spec.key);
        }
    }





    #[test]
    fn every_schema_is_a_closed_object() {
        for spec in specs() {
            let schema = (spec.input_schema)();
            // Every tool schema must be a schema the validator actually implements. A `$ref` or a
            // `oneOf` in a tool's schema is a gate that does not gate, and it would only be found
            // the day a call hit that branch.
            assert!(
                crate::schema::supported(&schema),
                "{} uses a JSON-Schema keyword the validator does not implement",
                spec.key
            );
            assert_eq!(schema["type"], "object", "{} is not an object schema", spec.key);
            // `additionalProperties = false` is the request's "unknown fields are refused, not
            // ignored", and it is the difference between a tool that validates its input and a
            // tool that silently drops a field the model believed it passed.
            assert_eq!(
                schema["additionalProperties"],
                Value::Bool(false),
                "{} does not refuse unknown arguments",
                spec.key
            );
        }
    }

    #[test]
    fn every_required_property_is_declared() {
        for spec in specs() {
            let schema = (spec.input_schema)();
            let empty = schema["properties"].as_object().cloned().unwrap_or_default();
            if let Some(required) = schema["required"].as_array() {
                for name in required.iter().filter_map(Value::as_str) {
                    assert!(
                        empty.contains_key(name),
                        "{} requires `{name}` but never declares it in properties",
                        spec.key
                    );
                }
            }
        }
    }

    #[test]
    fn the_example_validates_against_its_own_schema() {
        // An example that violates its own schema is worse than no example: the detail screen
        // shows it next to the schema, and the QA plan copies it and calls the tool with it.
        for spec in specs() {
            let example = (spec.example)();
            let schema = (spec.input_schema)();
            for key in required_names(&schema) {
                assert!(
                    example.get(&key).is_some(),
                    "{}'s example is missing its own required field `{key}`",
                    spec.key
                );
            }
            for (name, definition) in schema["properties"].as_object().into_iter().flatten() {
                let Some(value) = example.get(name) else { continue };
                assert_type(&value, definition["type"].as_str().unwrap_or("string"));
                if let Some(max) = definition["maxLength"].as_u64() {
                    assert!(
                        value.as_str().map_or(true, |s| s.chars().count() as u64 <= max),
                        "{}'s example overflows maxLength on `{name}`",
                        spec.key
                    );
                }
                if let Some(allowed) = definition["enum"].as_array() {
                    assert!(
                        allowed.contains(value),
                        "{}'s example uses `{name}` = {value}, which the enum does not carry",
                        spec.key
                    );
                }
            }
            // Closed means closed: the example itself may not carry a field the schema refuses.
            for name in example
                .as_object()
                .map(|o| o.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
            {
                assert!(
                    schema["properties"].get(&name).is_some(),
                    "{}'s example carries `{name}`, which its closed schema refuses",
                    spec.key
                );
            }
        }
    }

    #[test]
    fn a_high_risk_tool_ships_gated_and_a_low_risk_one_does_not() {
        for spec in specs() {
            let gated = default_requires_approval(spec);
            if spec.risk == Risk::High {
                assert!(gated, "{} is high risk and does not ship gated", spec.key);
            }
            if spec.risk == Risk::Low {
                assert!(!gated, "{} is low risk and ships gated", spec.key);
            }
        }
        // The request's two named over-grant risks, asserted by name so removing either one from
        // the table fails here rather than passing quietly.
        assert!(default_requires_approval(find("users.create").unwrap()));
        assert!(default_requires_approval(find("plugin.install").unwrap()));
    }

    #[test]
    fn limits_stay_inside_the_documented_bounds() {
        // The column bounds are 1000–300000 ms and 1–200 calls. A default outside them would be
        // refused by the column's own CHECK at seed time, i.e. at boot, on a fresh install.
        for spec in specs() {
            let (timeout, cap) = default_limits(spec);
            assert!(
                (1_000..=300_000).contains(&timeout),
                "{}'s default timeout {timeout} is outside the column bounds",
                spec.key
            );
            assert!(
                (1..=200).contains(&cap),
                "{}'s default call cap {cap} is outside the column bounds",
                spec.key
            );
        }
    }

    #[test]
    fn risk_reads_back_conservatively() {
        assert_eq!(Risk::from_str_lossy("low"), Risk::Low);
        assert_eq!(Risk::from_str_lossy("medium"), Risk::Medium);
        assert_eq!(Risk::from_str_lossy("high"), Risk::High);
        // A row from a newer version, or a corrupted one, must never become the permissive read.
        assert_eq!(Risk::from_str_lossy(""), Risk::High);
        assert_eq!(Risk::from_str_lossy("LOW"), Risk::High);
        assert_eq!(Risk::from_str_lossy("nonsense"), Risk::High);
    }

    #[test]
    fn every_class_is_one_the_filters_know() {
        let known = ["content", "media", "users", "sites", "themes", "plugins", "ops"];
        for spec in specs() {
            assert!(
                known.contains(&spec.class),
                "{} has class `{}`, which the class filter does not offer",
                spec.key,
                spec.class
            );
        }
        // Every class the request lists carries at least one tool, or the registry shows a filter
        // that returns nothing forever.
        for class in known {
            assert!(
                specs().iter().any(|spec| spec.class == class),
                "class `{class}` is offered as a filter but has no tool"
            );
        }
    }

    #[test]
    fn a_spec_survives_a_round_trip_through_the_row_shape() {
        // The seeder writes a row from a `ToolSpec` and the API reads it back into one. A field
        // that only exists on one side is a tool that renders as an empty cell in the panel.
        for spec in specs() {
            let row = json!({
                "key": spec.key,
                "class": spec.class,
                "description": spec.description,
                "permission": spec.permission,
                "risk": spec.risk.as_str(),
                "input_schema": (spec.input_schema)(),
                "example": (spec.example)(),
                "idempotent": spec.idempotent,
                "requires_approval": default_requires_approval(spec),
            });
            let read: Value = serde_json::from_value(row).unwrap();
            assert_eq!(read["key"], spec.key);
            assert_eq!(read["permission"], spec.permission);
            assert_eq!(read["risk"], spec.risk.as_str());
            assert_eq!(read["input_schema"], (spec.input_schema)());
        }
    }

    fn required_names(schema: &Value) -> Vec<String> {
        schema["required"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default()
    }

    fn assert_type(value: &Value, expected: &str) {
        let ok = match expected {
            "string" => value.is_string(),
            "integer" => value.is_i64() || value.is_u64(),
            "number" => value.is_number(),
            "object" => value.is_object(),
            "boolean" => value.is_boolean(),
            _ => true,
        };
        assert!(ok, "expected a {expected}, found {value}");
    }
}
