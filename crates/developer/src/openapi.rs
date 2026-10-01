//! The OpenAPI document the platform serves to its own API Explorer (REQ-033, slice 2).
//!
//! # Why this is a table and not a reflection of the router
//!
//! axum's `Router` has no public route iterator, so a document built by walking the running
//! router could only ever be a list of paths and methods with no schemas, no summaries and no
//! permission names — which is the half the Explorer exists for. The alternative (annotating
//! every handler with `#[utoipa::path]`) would mean touching every route in the application,
//! and every one of those routes belongs to a different wave. So the document is a **declared**
//! table, and [`crate::openapi::OPERATIONS`] is the only place an operation is written down.
//!
//! # Why a declared table cannot quietly rot
//!
//! A hand-written document next to a hand-written router is drift waiting to happen, and the
//! request file is explicit that "a CI check fails when the document drifts from the running
//! router" is an acceptance criterion rather than a nice-to-have. So the check exists and it
//! is *structural* rather than advisory:
//!
//! * `apps/api/tests/explorer_openapi.rs` parses `apps/api/src/routes/mod.rs` and rebuilds the
//!   set of `(method, path)` the router actually mounts.
//! * Every operation in [`OPERATIONS`] must be in that set — a document that describes a route
//!   which does not exist is worse than no document, because the Explorer's `Send` button would
//!   404 a call the reference promised.
//! * Every mounted `(method, path)` must be either documented or listed in
//!   [`UNDOCUMENTED_BASELINE`]. A **new** route is therefore a test failure until it is
//!   documented — which is the whole point of a drift gate: it fires on the commit that adds the
//!   route, not on the release that ships the wrong reference.
//!
//! Re-baselining (after documenting a batch, or after deliberately leaving one undocumented) is
//! an explicit edit to [`UNDOCUMENTED_BASELINE`] with a reason, not a flag.
//!
//! # The permission on an operation is the permission the *route guard* checks
//!
//! [`Operation::permission`] is not documentation. It is what the Explorer resolves **before**
//! dispatching, so that a call the caller could not make from the panel fails with the same
//! `403` and the same permission name before any handler runs. The real guard still runs
//! afterwards — the Explorer dispatches through the actual router with the caller's own session,
//! so this field is a fast, explanatory pre-check and never a substitute.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Where a parameter lives in the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum In {
    /// A path segment, written `{id}` in the path template.
    Path,
    /// A query string value.
    Query,
}

impl In {
    /// The OpenAPI spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::Query => "query",
        }
    }
}

/// The slice of JSON Schema the Explorer's form needs.
///
/// Deliberately not a general schema implementation. A developer who is handed
/// `oneOf`/`anyOf`/`not` and a form that cannot render them learns nothing, so the subset here
/// is the one the panel can draw, and an operation that needs more says so with
/// [`Schema::description`] rather than by emitting a construct the form silently drops.
#[derive(Debug, Clone, Serialize)]
pub struct Schema {
    /// The JSON type: `string`, `integer`, `boolean`, `object`, `array`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// The closed set of values, when there is one.
    #[serde(rename = "enum", skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<String>>,
    /// A `string` format hint (`uuid`, `date-time`).
    #[serde(rename = "format", skip_serializing_if = "Option::is_none")]
    pub format: Option<&'static str>,
    /// The child schema of an `array`.
    #[serde(rename = "items", skip_serializing_if = "Option::is_none")]
    pub items: Option<Box<Schema>>,
    /// The properties of an `object`.
    #[serde(rename = "properties", skip_serializing_if = "Option::is_none")]
    pub properties: Option<Vec<Property>>,
    /// Which properties of an `object` must be present.
    #[serde(rename = "required", skip_serializing_if = "Vec::is_empty")]
    pub required: Vec<String>,
    /// Extra prose the form shows under the field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<&'static str>,
}

impl Schema {
    /// A free-text string.
    #[must_use]
    pub const fn string() -> Self {
        Self::plain("string")
    }

    /// A string with a format hint.
    #[must_use]
    pub const fn formatted(kind: &'static str, format: &'static str) -> Self {
        Self {
            kind,
            format: Some(format),
            values: None,
            items: None,
            properties: None,
            required: Vec::new(),
            description: None,
        }
    }

    /// A string restricted to a closed set.
    #[must_use]
    pub fn enumeration(values: &[&str]) -> Self {
        Self {
            kind: "string",
            values: Some(values.iter().map(|value| (*value).to_owned()).collect()),
            format: None,
            items: None,
            properties: None,
            required: Vec::new(),
            description: None,
        }
    }

    /// A whole number.
    #[must_use]
    pub const fn integer() -> Self {
        Self::plain("integer")
    }

    /// A boolean.
    #[must_use]
    pub const fn boolean() -> Self {
        Self::plain("boolean")
    }

    /// A list of `item`s.
    #[must_use]
    pub fn array_of(item: Schema) -> Self {
        Self {
            kind: "array",
            values: None,
            format: None,
            items: Some(Box::new(item)),
            properties: None,
            required: Vec::new(),
            description: None,
        }
    }

    /// An object with named properties.
    #[must_use]
    pub fn object(properties: Vec<Property>, required: &[&str]) -> Self {
        Self {
            kind: "object",
            values: None,
            format: None,
            items: None,
            properties: Some(properties),
            required: required.iter().map(|name| (*name).to_string()).collect(),
            description: None,
        }
    }

    const fn plain(kind: &'static str) -> Self {
        Self {
            kind,
            values: None,
            format: None,
            items: None,
            properties: None,
            required: Vec::new(),
            description: None,
        }
    }

    /// With a note under the field.
    #[must_use]
    pub const fn described(mut self, description: &'static str) -> Self {
        self.description = Some(description);
        self
    }

    /// As a JSON Schema fragment for the document.
    #[must_use]
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({ "type": "string" }))
    }
}

/// One named field of an object schema.
#[derive(Debug, Clone, Serialize)]
pub struct Property {
    /// The field name.
    pub name: &'static str,
    /// Its schema.
    #[serde(flatten)]
    pub schema: Schema,
}

impl Property {
    /// A field of a given schema.
    #[must_use]
    pub const fn new(name: &'static str, schema: Schema) -> Self {
        Self { name, schema }
    }
}

/// A path or query parameter.
#[derive(Debug, Clone, Serialize)]
pub struct Parameter {
    /// The name, exactly as it appears in the path or query string.
    pub name: &'static str,
    /// Where it lives.
    #[serde(rename = "in")]
    pub location: In,
    /// Whether the route refuses the call without it.
    pub required: bool,
    /// What it means.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<&'static str>,
    /// Its type.
    #[serde(flatten)]
    pub schema: Schema,
}

impl Parameter {
    /// A required path segment — the only kind of path parameter that is ever required.
    #[must_use]
    pub const fn path(name: &'static str, schema: Schema) -> Self {
        Self {
            name,
            location: In::Path,
            required: true,
            description: None,
            schema,
        }
    }

    /// A query parameter.
    #[must_use]
    pub const fn query(name: &'static str, schema: Schema) -> Self {
        Self {
            name,
            location: In::Query,
            required: false,
            description: None,
            schema,
        }
    }

    /// With prose under the field.
    #[must_use]
    pub const fn described(mut self, description: &'static str) -> Self {
        self.description = Some(description);
        self
    }
}

/// One operation in the document.
#[derive(Debug, Clone, Serialize)]
pub struct Operation {
    /// The HTTP method, uppercase.
    pub method: &'static str,
    /// The full path, starting `/api/v1`, with `{name}` for path parameters.
    pub path: &'static str,
    /// The tag the Explorer groups the operation under.
    pub tag: &'static str,
    /// One line naming what it does, in product language.
    pub summary: &'static str,
    /// The permission the route guard checks — see the module comment.
    pub permission: Option<&'static str>,
    /// Path and query parameters.
    pub parameters: Vec<Parameter>,
    /// The request body, when the operation takes one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Schema>,
}

impl Operation {
    /// A read, with no body and no parameters beyond what is written.
    #[must_use]
    pub const fn read(
        method: &'static str,
        path: &'static str,
        tag: &'static str,
        summary: &'static str,
        permission: Option<&'static str>,
    ) -> Self {
        Self {
            method,
            path,
            tag,
            summary,
            permission,
            parameters: Vec::new(),
            body: None,
        }
    }

    /// With parameters.
    #[must_use]
    pub fn with_parameters(mut self, parameters: Vec<Parameter>) -> Self {
        self.parameters = parameters;
        self
    }

    /// With a request body.
    #[must_use]
    pub fn with_body(mut self, body: Schema) -> Self {
        self.body = Some(body);
        self
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers for the table
// ---------------------------------------------------------------------------------------------

/// A `{id}` path parameter — the only path parameter the platform uses, and it is always a UUID.
const fn id_param() -> Parameter {
    Parameter {
        name: "id",
        location: In::Path,
        required: true,
        description: Some("The row's identifier."),
        schema: Schema::formatted("string", "uuid"),
    }
}

/// A `?limit=` parameter.
const fn limit_param() -> Parameter {
    Parameter {
        name: "limit",
        location: In::Query,
        required: false,
        description: Some("How many rows to return."),
        schema: Schema::integer(),
    }
}

/// The permission the *sign-in* routes are behind, or `None` for the open ones.
///
/// `None` is a real value and not a placeholder: the sign-in routes are the only operations in
/// the document with no permission, and the Explorer refuses to run them (see
/// [`crate::openapi::is_refusable`] reasoning in `apps/api/src/explorer.rs`) because a call that
/// ends the session it is running as is not a call anybody wants from a form.
const NO_PERMISSION: Option<&str> = None;

// ---------------------------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------------------------

/// Every operation the Explorer documents.
///
/// Ordered by tag, then by path, so the document is diffable and a reviewer sees a moved line
/// rather than a reshuffled one. The *document* sorts itself at build time anyway (see
/// [`document`]), so this ordering is for humans reading the source.
pub static OPERATIONS: std::sync::LazyLock<Vec<Operation>> = std::sync::LazyLock::new(|| {
    vec![
        // --- Developer · keys and logs (slice 1) -------------------------------------------------
        Operation::read(
            "GET",
            "/api/v1/api-keys",
            "Developer",
            "List the organization's API keys",
            Some("developer.keys.read"),
        )
        .with_parameters(vec![Parameter::query(
            "environment",
            Schema::enumeration(&["live", "sandbox"]),
        )])
        .with_body(Schema::object(vec![], &[])),
        Operation::read(
            "POST",
            "/api/v1/api-keys",
            "Developer",
            "Mint a key; the secret is returned exactly once",
            Some("developer.keys.manage"),
        )
        .with_body(Schema::object(
            vec![
                Property::new(
                    "name",
                    Schema::string().described("3–60 characters, unique here."),
                ),
                Property::new(
                    "scopes",
                    Schema::array_of(Schema::string()).described("At least one permission key."),
                ),
                Property::new("environment", Schema::enumeration(&["live", "sandbox"])),
                Property::new(
                    "rate_tier",
                    Schema::enumeration(&["standard", "high"])
                        .described("`high` needs an owner or administrator."),
                ),
                Property::new(
                    "ip_allowlist",
                    Schema::array_of(Schema::string().described("A CIDR block.")),
                ),
                Property::new(
                    "expires_in_days",
                    Schema::integer().described("30, 90 or 365. Omit for no expiry."),
                ),
            ],
            &["name", "scopes", "environment"],
        )),
        Operation::read(
            "GET",
            "/api/v1/api-keys/{id}",
            "Developer",
            "One key with its daily usage",
            Some("developer.keys.read"),
        )
        .with_parameters(vec![
            id_param(),
            Parameter::query("days", Schema::integer()),
        ]),
        Operation::read(
            "DELETE",
            "/api/v1/api-keys/{id}",
            "Developer",
            "Revoke a key",
            Some("developer.keys.manage"),
        )
        .with_parameters(vec![id_param()]),
        Operation::read(
            "POST",
            "/api/v1/api-keys/{id}/rotate",
            "Developer",
            "Rotate a key; the old secret stops working immediately",
            Some("developer.keys.manage"),
        )
        .with_parameters(vec![id_param()]),
        Operation::read(
            "GET",
            "/api/v1/request-logs",
            "Developer",
            "The request log, filtered",
            Some("developer.keys.read"),
        )
        .with_parameters(vec![
            Parameter::query("api_key_id", Schema::formatted("string", "uuid")),
            Parameter::query("status", Schema::integer()),
            Parameter::query(
                "status_class",
                Schema::enumeration(&["1xx", "2xx", "3xx", "4xx", "5xx"]),
            ),
            Parameter::query("path_prefix", Schema::string()),
            Parameter::query(
                "method",
                Schema::enumeration(&["GET", "POST", "PUT", "PATCH", "DELETE"]),
            ),
            limit_param(),
            Parameter::query("offset", Schema::integer()),
        ]),
        Operation::read(
            "GET",
            "/api/v1/request-logs/{id}",
            "Developer",
            "One request's metadata. Bodies are never stored.",
            Some("developer.keys.read"),
        )
        .with_parameters(vec![Parameter {
            name: "id",
            location: In::Path,
            required: true,
            description: Some("The log row id."),
            schema: Schema::integer(),
        }]),
        // --- Developer · the Explorer itself (slice 2) --------------------------------------------
        Operation::read(
            "GET",
            "/api/v1/dev/openapi.json",
            "Developer",
            "This document",
            Some("developer.read"),
        ),
        Operation::read(
            "POST",
            "/api/v1/dev/explorer/requests",
            "Developer",
            "Run one API call as the signed-in caller",
            Some("developer.explorer.run"),
        )
        .with_body(Schema::object(
            vec![
                Property::new(
                    "method",
                    Schema::enumeration(&["GET", "POST", "PUT", "PATCH", "DELETE"]),
                ),
                Property::new(
                    "path",
                    Schema::string().described("An absolute `/api/v1/...` path."),
                ),
                Property::new(
                    "query",
                    Schema::array_of(Schema::string().described("A `name=value` pair.")),
                ),
                Property::new(
                    "body",
                    Schema::string().described("Raw request body, as text."),
                ),
            ],
            &["method", "path"],
        )),
        // --- Content ------------------------------------------------------------------------------
        Operation::read(
            "GET",
            "/api/v1/pages",
            "Content",
            "List pages",
            Some("content.pages.read"),
        )
        .with_parameters(vec![
            limit_param(),
            Parameter::query("offset", Schema::integer()),
        ]),
        Operation::read(
            "POST",
            "/api/v1/pages",
            "Content",
            "Create a page",
            Some("content.pages.create"),
        )
        .with_body(Schema::object(
            vec![
                Property::new("title", Schema::string()),
                Property::new("slug", Schema::string()),
                Property::new("site_id", Schema::formatted("string", "uuid")),
                Property::new("body", Schema::string()),
            ],
            &["title"],
        )),
        Operation::read(
            "GET",
            "/api/v1/pages/{id}",
            "Content",
            "One page",
            Some("content.pages.read"),
        )
        .with_parameters(vec![id_param()]),
        Operation::read(
            "PATCH",
            "/api/v1/pages/{id}",
            "Content",
            "Edit a page",
            Some("content.pages.update"),
        )
        .with_parameters(vec![id_param()])
        .with_body(Schema::object(vec![], &[])),
        Operation::read(
            "DELETE",
            "/api/v1/pages/{id}",
            "Content",
            "Delete a page",
            Some("content.pages.delete"),
        )
        .with_parameters(vec![id_param()]),
        Operation::read(
            "POST",
            "/api/v1/pages/{id}/publish",
            "Content",
            "Publish or unpublish a page",
            Some("content.pages.publish"),
        )
        .with_parameters(vec![id_param()]),
        // --- Tenancy -------------------------------------------------------------------------------
        Operation::read(
            "GET",
            "/api/v1/organizations",
            "Tenancy",
            "Organizations this account can see",
            Some("organizations.read"),
        ),
        Operation::read(
            "GET",
            "/api/v1/sites",
            "Tenancy",
            "List sites",
            Some("sites.read"),
        )
        .with_parameters(vec![limit_param()]),
        Operation::read(
            "GET",
            "/api/v1/sites/{id}",
            "Tenancy",
            "One site",
            Some("sites.read"),
        )
        .with_parameters(vec![id_param()]),
        // --- Identity ------------------------------------------------------------------------------
        Operation::read(
            "GET",
            "/api/v1/me",
            "Identity",
            "The signed-in account, its roles and its permissions",
            NO_PERMISSION,
        ),
        Operation::read(
            "GET",
            "/api/v1/iam/roles",
            "Identity",
            "List roles",
            Some("iam.roles.read"),
        ),
        Operation::read(
            "GET",
            "/api/v1/iam/permissions",
            "Identity",
            "The permission catalogue",
            Some("iam.permissions.read"),
        ),
        Operation::read(
            "GET",
            "/api/v1/iam/users",
            "Identity",
            "List accounts",
            Some("iam.users.read"),
        )
        .with_parameters(vec![limit_param()]),
    ]
});

/// Mounted routes the document deliberately does not describe yet.
///
/// **Every entry is debt, and this list is where it is written down.** Slice 2 documents the
/// developer surface and a small, useful slice of content, tenancy and identity — enough for the
/// Explorer to be a working tool rather than a list of six rows. The rest of the application
/// (roughly 330 operations across the waves other writers own) is listed here so that the drift
/// test can distinguish "known, not yet documented" from "added today and forgotten".
///
/// The test fails on a `(method, path)` that is mounted and *not* in this list, which is the
/// behaviour the request asks for: adding a route without documenting it breaks CI. Removing a
/// route also fails the test, because an entry here that is no longer mounted is a stale
/// exemption — and a stale exemption is how a "no untested screen" rule quietly stops testing.
pub const UNDOCUMENTED_BASELINE: &[&str] = &[
    "DELETE /api/v1/ai/providers/{id}",
    "DELETE /api/v1/auth/webauthn/passkeys/{factor_id}",
    "DELETE /api/v1/automations/{id}",
    "DELETE /api/v1/backup-schedules/{id}",
    "DELETE /api/v1/backups/{id}",
    "DELETE /api/v1/cdn/rules/{id}",
    "DELETE /api/v1/command-center/recent",
    "DELETE /api/v1/environments/{id}",
    "DELETE /api/v1/iam/bindings/{id}",
    "DELETE /api/v1/iam/devices/{id}",
    "DELETE /api/v1/iam/groups/{id}",
    "DELETE /api/v1/iam/policies/{id}",
    "DELETE /api/v1/iam/providers/{id}",
    "DELETE /api/v1/iam/provisioning/tokens/{id}",
    "DELETE /api/v1/iam/roles/{id}",
    "DELETE /api/v1/iam/service-accounts/{id}",
    "DELETE /api/v1/iam/service-accounts/{id}/keys/{key_id}",
    "DELETE /api/v1/iam/sessions/{id}",
    "DELETE /api/v1/iam/users/{id}/mfa/{factor_id}",
    "DELETE /api/v1/media/files/{id}",
    "DELETE /api/v1/media/folders/{id}",
    "DELETE /api/v1/media/grants/{grant_id}",
    "DELETE /api/v1/media/retention/{id}",
    "DELETE /api/v1/media/transformation-presets/{id}",
    "DELETE /api/v1/media/{id}",
    "DELETE /api/v1/media/{id}/shares/{share_id}",
    "DELETE /api/v1/notifications/{id}",
    "DELETE /api/v1/organizations/{id}",
    "DELETE /api/v1/organizations/{id}/departments/{department_id}",
    "DELETE /api/v1/organizations/{id}/departments/{department_id}/members",
    "DELETE /api/v1/organizations/{id}/departments/{department_id}/members/{user_id}",
    "DELETE /api/v1/organizations/{id}/departments/{department_id}/roles",
    "DELETE /api/v1/organizations/{id}/departments/{department_id}/roles/{binding_id}",
    "DELETE /api/v1/organizations/{id}/invitations/{invitation_id}",
    "DELETE /api/v1/organizations/{id}/members/{user_id}",
    "DELETE /api/v1/organizations/{id}/members/{user_id}/role-bindings/{binding_id}",
    "DELETE /api/v1/scim/v2/Groups/{id}",
    "DELETE /api/v1/scim/v2/Users/{id}",
    "DELETE /api/v1/search/recent",
    "DELETE /api/v1/sites/{id}",
    "DELETE /api/v1/sites/{id}/domains/{domain_id}",
    "DELETE /api/v1/webhooks/{id}",
    "DELETE /api/v1/workflows/{id}",
    "GET /api/v1/ai/models",
    "GET /api/v1/ai/providers",
    "GET /api/v1/analytics/export",
    "GET /api/v1/analytics/settings",
    "GET /api/v1/analytics/snippet",
    "GET /api/v1/auth/sso/providers",
    "GET /api/v1/auth/sso/{slug}/callback",
    "GET /api/v1/auth/sso/{slug}/saml",
    "GET /api/v1/auth/sso/{slug}/start",
    "GET /api/v1/auth/webauthn/passkeys",
    "GET /api/v1/automations",
    "GET /api/v1/automations/catalogue",
    "GET /api/v1/automations/{id}",
    "GET /api/v1/backup-schedules",
    "GET /api/v1/backup-settings",
    "GET /api/v1/backups",
    "GET /api/v1/backups/status",
    "GET /api/v1/backups/{id}",
    "GET /api/v1/backups/{id}/manifest",
    "GET /api/v1/backups/{id}/restore-jobs",
    "GET /api/v1/backups/{id}/restore-preview",
    "GET /api/v1/cdn/adapters",
    "GET /api/v1/cdn/purges",
    "GET /api/v1/cdn/purges/{id}",
    "GET /api/v1/cdn/rules",
    "GET /api/v1/cdn/rules/{id}",
    "GET /api/v1/cdn/settings",
    "GET /api/v1/cdn/status",
    "GET /api/v1/command-center/context",
    "GET /api/v1/command-center/recent",
    "GET /api/v1/commands",
    "GET /api/v1/deployment/checks",
    "GET /api/v1/deployment/cluster",
    "GET /api/v1/deployment/cluster/{environment}/samples/{workload}",
    "GET /api/v1/deployment/environments",
    "GET /api/v1/deployment/environments/{environment}",
    "GET /api/v1/deployment/history",
    "GET /api/v1/deployment/jobs/{id}",
    "GET /api/v1/deployment/jobs/{id}/log",
    "GET /api/v1/deployment/maintenance",
    "GET /api/v1/deployment/releases",
    "GET /api/v1/deployment/releases/{version}",
    "GET /api/v1/deployment/version",
    "GET /api/v1/dev/operations",
    "GET /api/v1/environments",
    "GET /api/v1/environments/{id}",
    "GET /api/v1/environments/{id}/changes",
    "GET /api/v1/environments/{id}/clone-jobs",
    "GET /api/v1/environments/{id}/promotions",
    "GET /api/v1/events",
    "GET /api/v1/events/catalogue",
    "GET /api/v1/events/retention",
    "GET /api/v1/iam/approvals",
    "GET /api/v1/iam/audit",
    "GET /api/v1/iam/bindings",
    "GET /api/v1/iam/devices",
    "GET /api/v1/iam/effective-permissions",
    "GET /api/v1/iam/groups",
    "GET /api/v1/iam/groups/{id}",
    "GET /api/v1/iam/overview",
    "GET /api/v1/iam/policies",
    "GET /api/v1/iam/policies/{id}",
    "GET /api/v1/iam/policies/{id}/versions",
    "GET /api/v1/iam/providers",
    "GET /api/v1/iam/providers/{id}",
    "GET /api/v1/iam/providers/{id}/events",
    "GET /api/v1/iam/provisioning/log",
    "GET /api/v1/iam/provisioning/tokens",
    "GET /api/v1/iam/requests",
    "GET /api/v1/iam/roles/{id}",
    "GET /api/v1/iam/roles/{id}/members",
    "GET /api/v1/iam/roles/{id}/versions",
    "GET /api/v1/iam/security-policies",
    "GET /api/v1/iam/service-accounts",
    "GET /api/v1/iam/service-accounts/{id}",
    "GET /api/v1/iam/sessions",
    "GET /api/v1/iam/users/{id}",
    "GET /api/v1/iam/users/{id}/mfa",
    "GET /api/v1/me/organizations",
    "GET /api/v1/media",
    "GET /api/v1/media/duplicates",
    "GET /api/v1/media/files",
    "GET /api/v1/media/files/{id}",
    "GET /api/v1/media/folders",
    "GET /api/v1/media/folders/{id}/grants",
    "GET /api/v1/media/grant-subjects",
    "GET /api/v1/media/quarantine",
    "GET /api/v1/media/retention",
    "GET /api/v1/media/retention/runs",
    "GET /api/v1/media/scan-settings",
    "GET /api/v1/media/scan/runs",
    "GET /api/v1/media/settings",
    "GET /api/v1/media/transformation-presets",
    "GET /api/v1/media/trash",
    "GET /api/v1/media/{id}",
    "GET /api/v1/media/{id}/activity",
    "GET /api/v1/media/{id}/grant-effective",
    "GET /api/v1/media/{id}/grants",
    "GET /api/v1/media/{id}/raw",
    "GET /api/v1/media/{id}/references",
    "GET /api/v1/media/{id}/shares",
    "GET /api/v1/media/{id}/versions",
    "GET /api/v1/media/{id}/versions/{version}/download",
    "GET /api/v1/media/{id}/versions/{version}/raw",
    "GET /api/v1/notifications",
    "GET /api/v1/notifications/channels",
    "GET /api/v1/notifications/preferences",
    "GET /api/v1/notifications/push-key",
    "GET /api/v1/notifications/summary",
    "GET /api/v1/notifications/{id}",
    "GET /api/v1/onboarding",
    "GET /api/v1/organizations/{id}",
    "GET /api/v1/organizations/{id}/audit",
    "GET /api/v1/organizations/{id}/departments",
    "GET /api/v1/organizations/{id}/departments/{department_id}",
    "GET /api/v1/organizations/{id}/invitations",
    "GET /api/v1/organizations/{id}/invitations/queue",
    "GET /api/v1/organizations/{id}/limits",
    "GET /api/v1/organizations/{id}/members",
    "GET /api/v1/organizations/{id}/members/{user_id}",
    "GET /api/v1/organizations/{id}/members/{user_id}/departments",
    "GET /api/v1/organizations/{id}/modules",
    "GET /api/v1/organizations/{id}/settings",
    "GET /api/v1/organizations/{id}/usage",
    "GET /api/v1/pages/{id}/revisions",
    "GET /api/v1/pages/{id}/revisions/{revision_id}",
    "GET /api/v1/pages/{id}/revisions/{revision_id}/comments",
    "GET /api/v1/pages/{id}/revisions/{revision_id}/translations",
    "GET /api/v1/promotions/{id}",
    "GET /api/v1/public/media/shared/{token}",
    "GET /api/v1/public/media/{id}",
    "GET /api/v1/public/pages/{slug}",
    "GET /api/v1/scim/v2/Groups",
    "GET /api/v1/scim/v2/Groups/{id}",
    "GET /api/v1/scim/v2/Schemas",
    "GET /api/v1/scim/v2/ServiceProviderConfig",
    "GET /api/v1/scim/v2/Users",
    "GET /api/v1/scim/v2/Users/{id}",
    "GET /api/v1/search",
    "GET /api/v1/search/export",
    "GET /api/v1/search/recent",
    "GET /api/v1/search/settings",
    "GET /api/v1/search/status",
    "GET /api/v1/search/suggest",
    "GET /api/v1/sites/{id}/domains",
    "GET /api/v1/webhooks",
    "GET /api/v1/webhooks/{id}",
    "GET /api/v1/webhooks/{id}/deliveries",
    "GET /api/v1/webhooks/{id}/stats",
    "GET /api/v1/workflow-executions/{id}",
    "GET /api/v1/workflows",
    "GET /api/v1/workflows/{id}",
    "GET /api/v1/workflows/{id}/executions",
    "PATCH /api/v1/ai/models/{id}",
    "PATCH /api/v1/ai/providers/{id}",
    "PATCH /api/v1/iam/groups/{id}",
    "PATCH /api/v1/iam/providers/{id}",
    "PATCH /api/v1/iam/roles/{id}",
    "PATCH /api/v1/iam/users/{id}",
    "PATCH /api/v1/media/files/{id}",
    "PATCH /api/v1/media/folders/{id}",
    "PATCH /api/v1/media/transformation-presets/{id}",
    "PATCH /api/v1/organizations/{id}",
    "PATCH /api/v1/organizations/{id}/departments/{department_id}",
    "PATCH /api/v1/organizations/{id}/members/{user_id}",
    "PATCH /api/v1/organizations/{id}/members/{user_id}/role-bindings/{binding_id}",
    "PATCH /api/v1/scim/v2/Groups/{id}",
    "PATCH /api/v1/scim/v2/Users/{id}",
    "PATCH /api/v1/sites/{id}",
    "PATCH /api/v1/webhooks/{id}",
    "POST /api/v1/ai/chat",
    "POST /api/v1/ai/providers",
    "POST /api/v1/ai/providers/{id}/discover-models",
    "POST /api/v1/auth/login",
    "POST /api/v1/auth/logout",
    "POST /api/v1/auth/mfa/verify",
    "POST /api/v1/auth/step-up",
    "POST /api/v1/auth/webauthn/authenticate/begin",
    "POST /api/v1/auth/webauthn/authenticate/complete",
    "POST /api/v1/auth/webauthn/register/begin",
    "POST /api/v1/auth/webauthn/register/complete",
    "POST /api/v1/automations",
    "POST /api/v1/backup-schedules",
    "POST /api/v1/backup-schedules/{id}/run",
    "POST /api/v1/backups",
    "POST /api/v1/backups/sweep",
    "POST /api/v1/backups/{id}/restore",
    "POST /api/v1/backups/{id}/restore-queue",
    "POST /api/v1/backups/{id}/verify",
    "POST /api/v1/cdn/purges",
    "POST /api/v1/cdn/purges/{id}/retry",
    "POST /api/v1/cdn/rules",
    "POST /api/v1/cdn/rules/reorder",
    "POST /api/v1/cdn/rules/{id}/toggle",
    "POST /api/v1/cdn/settings/test",
    "POST /api/v1/command-center/recent",
    "POST /api/v1/command-center/resolve",
    "POST /api/v1/commands/{id}/run",
    "POST /api/v1/deployment/checks/run",
    "POST /api/v1/deployment/cluster/{environment}/restart",
    "POST /api/v1/deployment/cluster/{environment}/sample",
    "POST /api/v1/deployment/environments/{environment}/deploy",
    "POST /api/v1/deployment/environments/{environment}/preflight",
    "POST /api/v1/deployment/environments/{environment}/rollback",
    "POST /api/v1/deployment/jobs/{id}",
    "POST /api/v1/environments",
    "POST /api/v1/environments/{id}/clone",
    "POST /api/v1/environments/{id}/clone-jobs/{job_id}/cancel",
    "POST /api/v1/environments/{id}/promotions",
    "POST /api/v1/events/retention/sweep",
    "POST /api/v1/iam/approvals/{id}/decide",
    "POST /api/v1/iam/bindings",
    "POST /api/v1/iam/devices/{id}/trust",
    "POST /api/v1/iam/groups",
    "POST /api/v1/iam/policies",
    "POST /api/v1/iam/policies/{id}/test",
    "POST /api/v1/iam/providers",
    "POST /api/v1/iam/providers/{id}/test",
    "POST /api/v1/iam/provisioning/tokens",
    "POST /api/v1/iam/requests",
    "POST /api/v1/iam/roles",
    "POST /api/v1/iam/roles/{id}/duplicate",
    "POST /api/v1/iam/roles/{id}/preview",
    "POST /api/v1/iam/service-accounts",
    "POST /api/v1/iam/service-accounts/{id}/keys",
    "POST /api/v1/iam/simulations",
    "POST /api/v1/iam/users",
    "POST /api/v1/iam/users/{id}/mfa",
    "POST /api/v1/iam/users/{id}/mfa/{factor_id}/confirm",
    "POST /api/v1/iam/users/{id}/reset-mfa",
    "POST /api/v1/iam/users/{id}/sign-out-all",
    "POST /api/v1/me/organization",
    "POST /api/v1/media/bulk",
    "POST /api/v1/media/duplicates/merge",
    "POST /api/v1/media/files/{id}/purge",
    "POST /api/v1/media/files/{id}/restore",
    "POST /api/v1/media/folders",
    "POST /api/v1/media/quarantine/{id}/release",
    "POST /api/v1/media/retention",
    "POST /api/v1/media/retention/repair",
    "POST /api/v1/media/retention/run",
    "POST /api/v1/media/scan/run",
    "POST /api/v1/media/scan/test",
    "POST /api/v1/media/settings/test-connection",
    "POST /api/v1/media/transformation-presets",
    "POST /api/v1/media/trash/empty",
    "POST /api/v1/media/{id}/shares",
    "POST /api/v1/media/{id}/shares/revoke-all",
    "POST /api/v1/media/{id}/versions",
    "POST /api/v1/media/{id}/versions/{version}/restore",
    "POST /api/v1/notifications/bulk",
    "POST /api/v1/notifications/emit",
    "POST /api/v1/notifications/mark-all-read",
    "POST /api/v1/notifications/preferences/test",
    "POST /api/v1/notifications/{id}/read",
    "POST /api/v1/onboarding/ai-provider",
    "POST /api/v1/onboarding/complete",
    "POST /api/v1/onboarding/organization",
    "POST /api/v1/onboarding/owner",
    "POST /api/v1/onboarding/site",
    "POST /api/v1/onboarding/theme",
    "POST /api/v1/organizations",
    "POST /api/v1/organizations/{id}/departments",
    "POST /api/v1/organizations/{id}/departments/{department_id}",
    "POST /api/v1/organizations/{id}/departments/{department_id}/members",
    "POST /api/v1/organizations/{id}/departments/{department_id}/roles",
    "POST /api/v1/organizations/{id}/invitations",
    "POST /api/v1/organizations/{id}/invitations/{invitation_id}/release",
    "POST /api/v1/organizations/{id}/members",
    "POST /api/v1/organizations/{id}/members/{user_id}/role-bindings",
    "POST /api/v1/pages/{id}/restore",
    "POST /api/v1/promotions/{id}/approve",
    "POST /api/v1/promotions/{id}/cancel",
    "POST /api/v1/restore-jobs/{id}/cancel",
    "POST /api/v1/scim/v2/Groups",
    "POST /api/v1/scim/v2/Users",
    "POST /api/v1/search/reindex",
    "POST /api/v1/sites",
    "POST /api/v1/sites/{id}/domains",
    "POST /api/v1/sites/{id}/domains/{domain_id}/primary",
    "POST /api/v1/webhooks",
    "POST /api/v1/webhooks/{id}/deliveries/redeliver",
    "POST /api/v1/webhooks/{id}/deliveries/{delivery_id}/redeliver",
    "POST /api/v1/webhooks/{id}/secret/rotate",
    "POST /api/v1/webhooks/{id}/test",
    "POST /api/v1/workflow-executions/{id}/cancel",
    "POST /api/v1/workflows",
    "POST /api/v1/workflows/{id}/run",
    "PUT /api/v1/ai/providers/{id}/models",
    "PUT /api/v1/automations/{id}",
    "PUT /api/v1/backup-schedules/{id}",
    "PUT /api/v1/backup-settings",
    "PUT /api/v1/cdn/rules/{id}",
    "PUT /api/v1/cdn/settings",
    "PUT /api/v1/deployment/maintenance/{environment}",
    "PUT /api/v1/iam/groups/{id}/members",
    "PUT /api/v1/iam/policies/{id}",
    "PUT /api/v1/iam/roles/{id}/permissions",
    "PUT /api/v1/iam/security-policies",
    "PUT /api/v1/media/files/{id}/hold",
    "PUT /api/v1/media/folders/{id}/grants",
    "PUT /api/v1/media/retention/{id}",
    "PUT /api/v1/media/scan-settings",
    "PUT /api/v1/media/settings",
    "PUT /api/v1/media/{id}/grants",
    "PUT /api/v1/notifications/preferences",
    "PUT /api/v1/organizations/{id}/limits",
    "PUT /api/v1/organizations/{id}/modules",
    "PUT /api/v1/organizations/{id}/settings",
    "PUT /api/v1/pages/{id}/revisions/{revision_id}/translations/{language}",
    "PUT /api/v1/scim/v2/Users/{id}",
    "PUT /api/v1/workflows/{id}",
];

/// The served document, as a JSON value.
///
/// Built from [`OPERATIONS`] on every call rather than cached in a `OnceLock`, because the
/// document is a pure function of a `&'static` table and the cost is a few hundred
/// serialisations of a few hundred kilobytes at most — while a cache would need invalidation
/// for a table that only ever changes with a binary. A `LazyLock` is the one place this could
/// be made cheaper, and it is not worth the coupling until somebody measures it.
#[must_use]
pub fn document() -> Value {
    let mut operations = OPERATIONS.clone();
    // Stable order: tag, then path, then method. Sorting here rather than trusting the source
    // order means a reviewer who adds an operation in the wrong place does not produce a diff
    // that reorders the whole file.
    operations.sort_by(|left, right| {
        left.tag
            .cmp(right.tag)
            .then_with(|| left.path.cmp(right.path))
            .then_with(|| left.method.cmp(right.method))
    });

    let mut paths: serde_json::Map<String, Value> = serde_json::Map::new();
    for operation in &operations {
        let entry = paths
            .entry(operation.path.to_owned())
            .or_insert_with(|| json!({}));
        let mut parameters = Vec::new();
        for parameter in &operation.parameters {
            let mut value = serde_json::to_value(parameter).unwrap_or_else(|_| json!({}));
            if let Some(object) = value.as_object_mut() {
                // `name` and `in` live beside the flattened schema, which is exactly the OpenAPI
                // 3 shape. `required` is only meaningful for a path parameter, and emitting it
                // for a query parameter is a validation error in most linters.
                if parameter.location == In::Query {
                    object.remove("required");
                }
            }
            parameters.push(value);
        }

        let mut spec = json!({
            "summary": operation.summary,
            "operationId": operation_id(operation),
            "tags": [operation.tag],
            "x-omnion-permission": operation.permission,
        });
        if !parameters.is_empty() {
            spec["parameters"] = Value::Array(parameters);
        }
        if let Some(body) = &operation.body {
            spec["requestBody"] = json!({
                "required": true,
                "content": {
                    "application/json": { "schema": body.to_value() },
                },
            });
        }
        spec["responses"] = json!({
            "200": { "description": "The operation's answer." },
            "400": { "description": "The request is unusable." },
            "401": { "description": "Not signed in." },
            "403": { "description": "Signed in, but not permitted." },
        });

        if let Some(object) = entry.as_object_mut() {
            let verb = operation.method.to_ascii_lowercase();
            if object.contains_key(&verb) {
                // Two operations claiming the same method and path is a table bug, and letting
                // the later one win would make the document quietly describe whichever entry
                // happened to be second. The drift test asserts there is no duplicate, so this
                // branch is a backstop rather than a behaviour.
                object.insert(
                    verb,
                    json!({"summary": "DUPLICATE OPERATION — see the drift test"}),
                );
            } else {
                object.insert(verb, spec);
            }
        }
    }

    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Omnion API",
            "version": env!("CARGO_PKG_VERSION"),
            "description":
                "The operations this installation documents. Every path is served by the same \
                 router the panel uses, and the Explorer runs each call as the signed-in caller.",
        },
        "servers": [{ "url": "/", "description": "This installation" }],
        "tags": tags(),
        "paths": paths,
    })
}

/// The tag list, in the order the Explorer groups operations.
fn tags() -> Value {
    let mut seen: Vec<&str> = Vec::new();
    for operation in OPERATIONS.iter() {
        if !seen.contains(&operation.tag) {
            seen.push(operation.tag);
        }
    }
    Value::Array(seen.into_iter().map(|tag| json!({ "name": tag })).collect())
}

/// A stable operation id, derived from the method and the path.
///
/// Derived rather than hand-written because a hand-written id is a third thing to keep in sync
/// (method, path, id) and the snippet drawer needs one to address an operation by.
/// `POST /api/v1/api-keys/{id}/rotate` becomes `postApiKeysIdRotate`: the `api/v1` prefix is
/// dropped because every operation in the document carries it, so including it would make
/// every id longer without making any of them more unique. The `{`, `}` and `_` markers become
/// word breaks, so a parameter reads as a word rather than as punctuation.
#[must_use]
pub fn operation_id(operation: &Operation) -> String {
    let path = operation
        .path
        .strip_prefix("/api/v1/")
        .unwrap_or(operation.path);
    let mut id = String::from(operation.method.to_ascii_lowercase());
    for segment in path.split('/') {
        if segment.is_empty() {
            continue;
        }
        id.push_str(&camel_case(segment));
    }
    id
}

/// Turn one path segment into a capitalised word.
///
/// Every segment **starts** capitalised and capitalises again after each marker, so `api-keys`
/// becomes `ApiKeys` and `rotate` becomes `Rotate`. Only capitalising after a marker would leave
/// every single-word segment lowercase and produce `postapiKeysIdrotate`, which is mechanically
/// correct and unreadable — and an id a person has to type is an id a person will not.
fn camel_case(segment: &str) -> String {
    let mut out = String::new();
    let mut upper_next = true;
    for character in segment.chars() {
        if character == '{' || character == '}' || character == '-' || character == '_' {
            upper_next = true;
            continue;
        }
        if upper_next {
            out.extend(character.to_uppercase());
            upper_next = false;
        } else {
            out.push(character);
        }
    }
    out
}

/// The operation at `method` and `path`, if the document describes one.
#[must_use]
pub fn find(method: &str, path: &str) -> Option<&'static Operation> {
    // `LazyLock` derefs to a `&'static Vec<Operation>` for the process's lifetime, so the
    // borrow below really is `'static` — the table is never rebuilt and never dropped.
    let method = method.to_ascii_uppercase();
    OPERATIONS
        .iter()
        .find(|operation| operation.method == method && operation.path == path)
}

/// The operations a caller may *see*, given the permission keys they hold.
///
/// The filter is on the **documented** permission, and a caller's effective permission set
/// arrives from the same resolution the route guard uses — so a role that cannot publish a page
/// does not see a `POST /pages/{id}/publish` button it would only be refused by. An operation
/// with no permission is visible to anybody who can reach the Explorer at all.
#[must_use]
pub fn visible_to<'a, F>(operations: &'a [Operation], holds: F) -> Vec<&'a Operation>
where
    F: Fn(&str) -> bool,
{
    operations
        .iter()
        .filter(|operation| match operation.permission {
            None => true,
            Some(permission) => holds(permission),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn every_operation_has_a_summary_and_a_tag() {
        // A row with an empty summary is a row the Explorer's browser shows as a bare path, which
        // is the shape of a reference nobody reads.
        for operation in OPERATIONS.iter() {
            assert!(
                !operation.summary.trim().is_empty(),
                "{} {} has no summary",
                operation.method,
                operation.path
            );
            assert!(
                !operation.tag.trim().is_empty(),
                "{} {} has no tag",
                operation.method,
                operation.path
            );
        }
    }

    #[test]
    fn every_path_starts_with_the_versioned_prefix() {
        // A path outside `/api/v1` is a path the Explorer would send and the router would
        // answer 404 for, and the drift test would not catch it because the test also reads the
        // router — both would agree on a path that no client can reach.
        for operation in OPERATIONS.iter() {
            assert!(
                operation.path.starts_with("/api/v1/"),
                "{} is not under /api/v1",
                operation.path
            );
        }
    }

    #[test]
    fn no_two_operations_share_a_method_and_a_path() {
        let mut seen = BTreeSet::new();
        for operation in OPERATIONS.iter() {
            let key = (operation.method, operation.path);
            assert!(
                seen.insert(key),
                "{} {} is documented twice",
                operation.method,
                operation.path
            );
        }
    }

    #[test]
    fn every_path_parameter_is_declared_and_named_in_the_path() {
        // The reverse case is the one that produces a form with a field the route never reads.
        for operation in OPERATIONS.iter() {
            for parameter in &operation.parameters {
                if parameter.location == In::Path {
                    assert!(
                        operation.path.contains(&format!("{{{}}}", parameter.name)),
                        "{} declares a path parameter {} the path does not carry",
                        operation.path,
                        parameter.name
                    );
                } else {
                    assert!(
                        !operation.path.contains(&format!("{{{}}}", parameter.name)),
                        "{} calls {} a query parameter but the path interpolates it",
                        operation.path,
                        parameter.name
                    );
                }
            }
        }
    }

    #[test]
    fn a_required_object_property_is_also_present_in_the_schema() {
        // `required: ["titel"]` beside `properties: [title]` is a form that cannot be satisfied
        // and a schema that fails validation, and neither error is readable.
        for operation in OPERATIONS.iter() {
            let Some(body) = &operation.body else {
                continue;
            };
            let Some(properties) = &body.properties else {
                continue;
            };
            for name in &body.required {
                assert!(
                    properties.iter().any(|property| property.name == *name),
                    "{} requires {name}, which is not one of its properties",
                    operation.path
                );
            }
        }
    }

    #[test]
    fn an_enumeration_is_never_empty() {
        // `enum: []` matches nothing, so a required field with that schema can never be filled
        // in and the only way to send the call is to bypass the form.
        fn walk(schema: &Schema) {
            if let Some(values) = &schema.values {
                assert!(
                    !values.is_empty(),
                    "an enumeration must offer at least one value"
                );
            }
            if let Some(item) = &schema.items {
                walk(item);
            }
            if let Some(properties) = &schema.properties {
                for property in properties {
                    walk(&property.schema);
                }
            }
        }
        for operation in OPERATIONS.iter() {
            for parameter in &operation.parameters {
                walk(&parameter.schema);
            }
            if let Some(body) = &operation.body {
                walk(body);
            }
        }
    }

    #[test]
    fn the_document_is_valid_json_with_one_entry_per_operation() {
        let document = document();
        let paths = document["paths"].as_object().expect("paths is an object");
        let expected = OPERATIONS.len();
        let described: usize = paths
            .values()
            .map(|item| item.as_object().map_or(0, |verbs| verbs.len()))
            .sum();
        assert_eq!(described, expected, "one document entry per operation");
        assert_eq!(document["openapi"], "3.1.0");
    }

    #[test]
    fn a_query_parameter_is_not_marked_required() {
        // OpenAPI's validator rejects `required: true` on a query parameter outright, and a
        // document that fails validation is a document a developer's editor refuses to load.
        let document = document();
        for (path, verbs) in document["paths"].as_object().expect("paths is an object") {
            for (verb, spec) in verbs.as_object().expect("verbs is an object") {
                let Some(parameters) = spec["parameters"].as_array() else {
                    continue;
                };
                for parameter in parameters {
                    if parameter["in"] == "query" {
                        assert!(
                            parameter.get("required").is_none(),
                            "{verb} {path}: a query parameter may not be required"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn operation_ids_are_unique_and_camel_cased() {
        let mut seen = BTreeSet::new();
        for operation in OPERATIONS.iter() {
            let id = operation_id(operation);
            assert!(seen.insert(id.clone()), "operation id {id} is used twice");
            assert!(
                !id.contains('{') && !id.contains('}') && !id.contains('_'),
                "{id} still carries a path marker"
            );
        }
    }

    #[test]
    fn an_operation_id_reads_back_from_the_path() {
        let operation = find("POST", "/api/v1/api-keys/{id}/rotate").expect("rotate is documented");
        assert_eq!(operation_id(operation), "postApiKeysIdRotate");
    }

    #[test]
    fn the_visibility_filter_hides_what_the_caller_cannot_do() {
        let holds = |permission: &str| permission != "content.pages.publish";
        let visible = visible_to(&OPERATIONS, holds);
        assert!(
            visible
                .iter()
                .all(|operation| operation.permission != Some("content.pages.publish")),
            "a permission the caller does not hold must not appear"
        );
        assert!(
            visible
                .iter()
                .any(|operation| operation.permission == Some("content.pages.read")),
            "a permission the caller does hold must appear"
        );
        // `GET /me` carries no permission and is therefore always visible: it is the caller's
        // own account, and hiding it would be hiding the answer to "why can I not".
        assert!(
            visible
                .iter()
                .any(|operation| operation.path == "/api/v1/me" && operation.permission.is_none())
        );
    }

    #[test]
    fn the_documented_permissions_are_ones_the_catalogue_knows() {
        // A permission name in the document that the catalogue does not hold is a route guard
        // that answers 403 for everybody — including the owner — and the Explorer would report
        // that as "your role is wrong" rather than "the reference is wrong".
        for operation in OPERATIONS.iter() {
            let Some(permission) = operation.permission else {
                continue;
            };
            assert!(
                omnion_permissions_permission_is_known(permission),
                "{permission} is not in the permission catalogue"
            );
        }
    }

    /// The catalogue lookup, kept local so this module needs no dependency on the IAM crate.
    ///
    /// A *local* list rather than a `omnion-permissions` dependency on purpose: the developer
    /// crate is infrastructure and a reference table has no business pulling the whole IAM
    /// dependency graph in behind it. The list below is the subset the document uses, and the
    /// assertion that keeps it honest is that the *test* above fails loudly when a new operation
    /// names a permission that is not in it.
    fn omnion_permissions_permission_is_known(permission: &str) -> bool {
        const KNOWN: &[&str] = &[
            "content.pages.create",
            "content.pages.delete",
            "content.pages.publish",
            "content.pages.read",
            "content.pages.update",
            "developer.explorer.run",
            "developer.keys.manage",
            "developer.keys.read",
            "developer.read",
            "iam.permissions.read",
            "iam.roles.read",
            "iam.users.read",
            "organizations.read",
            "sites.read",
        ];
        KNOWN.contains(&permission)
    }

    #[test]
    fn the_local_permission_list_is_exactly_what_the_document_uses() {
        // The list above is a copy, and a copy that drifts is a test that stops testing. This
        // one keeps the two in step: it fails when the document names a permission the list does
        // not hold, which is the same condition as the test above, and fails when the list holds
        // a permission nothing uses, which is the direction a copy rots in.
        let used: BTreeSet<&str> = OPERATIONS.iter().filter_map(|o| o.permission).collect();
        let known: BTreeSet<&str> = [
            "content.pages.create",
            "content.pages.delete",
            "content.pages.publish",
            "content.pages.read",
            "content.pages.update",
            "developer.explorer.run",
            "developer.keys.manage",
            "developer.keys.read",
            "developer.read",
            "iam.permissions.read",
            "iam.roles.read",
            "iam.users.read",
            "organizations.read",
            "sites.read",
        ]
        .into_iter()
        .collect();
        assert_eq!(
            used, known,
            "the local permission list and the document disagree"
        );
    }

    #[test]
    fn the_explorers_own_operations_are_documented() {
        // The two operations that make the Explorer work are the ones a drifted document would
        // most embarrassingly omit: the button that fetches the reference and the button that
        // sends the call.
        assert!(find("GET", "/api/v1/dev/openapi.json").is_some());
        assert!(find("POST", "/api/v1/dev/explorer/requests").is_some());
    }

    #[test]
    fn the_baseline_has_no_duplicates() {
        // Two identical baseline entries mean a route was removed and its exemption left
        // behind, which is the direction in which a "no untested screen" rule quietly dies.
        let mut seen = BTreeSet::new();
        for entry in UNDOCUMENTED_BASELINE {
            assert!(seen.insert(*entry), "{entry} is listed twice");
            assert!(
                entry.contains(" /api/v1/"),
                "{entry} is not a method and a path"
            );
        }
    }
}
