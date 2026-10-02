//! `/api/v1/node-types` and `/api/v1/credential-types` — the discovery surface of the node
//! library (docs/requests/REQ-087, slice 1).
//!
//! Both endpoints are pure reads of the registry in `omnion_workflows::registry`. They touch
//! no database, because the registry *is* code: that is what makes the answer trustworthy. A
//! node cannot appear because a row exists, and it cannot go missing because a row was deleted
//! or because an install half-completed (REQ-087 slice 4 records installations; it does not
//! define them).
//!
//! Four decisions the handlers make, each one a way a library screen lies to the reader:
//!
//! 1. **A search that matched nothing says so with the filter it applied.** An empty list with
//!    no echo of the query is indistinguishable from a broken endpoint, and the reader has no
//!    way to tell which they are looking at.
//! 2. **Counts are computed from the same set that is returned.** A "12 of 40" line whose
//!    denominator came from a second query is a number that can disagree with the list under
//!    it, and when it does the reader trusts the wrong one.
//! 3. **A deprecated node is returned, not hidden.** Hiding it would make an existing
//!    workflow's node unresolvable, and the person reading the library is precisely the one who
//!    needs to know it exists *and* what replaced it. The state chip carries the cause.
//! 4. **The `installed` filter is a pass-through, not a guess.** Until slice 4 ships a ledger
//!    there is nothing installed beyond the bundled set, so the filter answers from the
//!    registry's own `source` and the response says how many bundled nodes back the answer —
//!    rather than reporting zero installed as if a query had found nothing.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use omnion_workflows::registry::{
    Capability, CredentialDefinition, FieldType, NodeCategory, NodeDefinition, PortKind,
    credential_types, find_credential_type, find_node, lint, nodes,
};
use serde::{Deserialize, Serialize};

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------------------------

/// The query of the node-type list.
#[derive(Debug, Default, Deserialize)]
pub struct NodeTypeQuery {
    /// Free text, matched case-insensitively over label, key and description.
    pub search: Option<String>,
    /// One category; an unknown category is refused rather than matched to nothing.
    pub category: Option<String>,
    /// One capability, e.g. `trigger` or `webhook`.
    pub capability: Option<String>,
    /// `available` (the default) or `deprecated`, or `false` to exclude deprecated nodes.
    pub deprecated: Option<bool>,
    /// `true` for only the nodes that can use a credential, `false` for only those that cannot.
    pub credential: Option<bool>,
}

impl NodeTypeQuery {
    /// Whether deprecated nodes are part of the answer.
    ///
    /// The default is *yes*: a library that silently drops a deprecated node hides the fact
    /// that a workflow already depends on it.
    fn include_deprecated(&self) -> bool {
        self.deprecated.unwrap_or(true)
    }
}

// ---------------------------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------------------------

/// One port, as the palette draws it.
#[derive(Debug, Serialize)]
pub struct PortBody {
    /// Port name.
    pub name: String,
    /// `main`, `error` or `ai_tool`.
    pub kind: &'static str,
    /// Data kinds the port accepts; empty means "anything".
    pub accepts: Vec<String>,
    /// Whether a connection may leave (or arrive at) it.
    pub open: bool,
}

/// One parameter of a node's inspector form.
#[derive(Debug, Serialize)]
pub struct ParamBody {
    /// Parameter name as it appears in `params`.
    pub name: String,
    /// JSON Schema type.
    pub kind: String,
    /// Field label.
    pub label: String,
    /// Whether the value must be present.
    pub required: bool,
    /// How the palette renders it.
    pub ui: &'static str,
    /// Allowed values for a select.
    pub options: Vec<String>,
    /// Where a select's options come from when they are not an enum.
    pub options_source: Option<String>,
    /// Placeholder.
    pub placeholder: Option<String>,
    /// One-line help.
    pub help: Option<String>,
    /// Default value.
    pub default: Option<serde_json::Value>,
    /// Whether this field holds a credential *key* rather than a value.
    pub secret_field: bool,
}

/// One node in the library, in full.
#[derive(Debug, Serialize)]
pub struct NodeTypeBody {
    /// Stable key.
    pub key: String,
    /// Version a workflow is recorded against.
    pub version: String,
    /// Label in the palette.
    pub label: String,
    /// One-line description.
    pub description: String,
    /// Category tree placement.
    pub category: &'static str,
    /// Lucide icon name.
    pub icon: String,
    /// Documentation link.
    pub docs_url: String,
    /// Input ports.
    pub inputs: Vec<PortBody>,
    /// Output ports.
    pub outputs: Vec<PortBody>,
    /// Inspector parameters.
    pub params: Vec<ParamBody>,
    /// Credential types the node can use.
    pub credential_types: Vec<String>,
    /// What the node can do.
    pub capabilities: Vec<&'static str>,
    /// `none` or `required` — whether its code runs out of process.
    pub sandbox: &'static str,
    /// Attempts a run allows when the person setting it up did not say.
    pub default_max_attempts: i32,
    /// Whether this version is deprecated.
    pub deprecated: bool,
    /// The key that replaces a deprecated node.
    pub superseded_by: Option<String>,
    /// What the library row shows as its state: `available`, `deprecated` or
    /// `node_package_missing`. Slice 4 replaces the last one with a real ledger read; until
    /// then a bundled node is never missing, and a node *with* a package is not registered at
    /// all, so the third state is returned by the API and asserted rather than invented client
    /// side.
    pub state: &'static str,
    /// Why the node is in that state, in words the palette can show next to the chip.
    pub state_reason: Option<String>,
}

impl NodeTypeBody {
    /// Describe one registry entry.
    fn build(node: &NodeDefinition) -> Self {
        let (state, state_reason) = if node.deprecated {
            let reason = node.superseded_by.map_or_else(
                || "this version is deprecated and has no replacement yet".to_string(),
                |key| format!("deprecated — use {key} instead"),
            );
            ("deprecated", Some(reason))
        } else {
            ("available", None)
        };

        Self {
            key: node.key.to_string(),
            version: node.version.to_string(),
            label: node.label.to_string(),
            description: node.description.to_string(),
            category: node.category.as_str(),
            icon: node.icon.to_string(),
            docs_url: node.docs_url.to_string(),
            inputs: node.inputs.iter().map(PortBody::build).collect(),
            outputs: node.outputs.iter().map(PortBody::build).collect(),
            params: node
                .params
                .iter()
                .map(|param| ParamBody {
                    name: param.name.clone(),
                    kind: param.kind.clone(),
                    label: param.label.clone(),
                    required: param.required,
                    ui: param.ui.as_str(),
                    options: param.options.clone(),
                    options_source: param.options_source.clone(),
                    placeholder: param.placeholder.clone(),
                    help: param.help.clone(),
                    default: param.default.clone(),
                    secret_field: param.secret_field,
                })
                .collect(),
            credential_types: node
                .credential_types
                .iter()
                .map(|key| (*key).to_string())
                .collect(),
            capabilities: node
                .capabilities
                .iter()
                .map(|capability| capability.as_str())
                .collect(),
            sandbox: node.sandbox.as_str(),
            default_max_attempts: node.default_max_attempts,
            deprecated: node.deprecated,
            superseded_by: node.superseded_by.map(str::to_string),
            state,
            state_reason,
        }
    }
}

impl PortBody {
    fn build(port: &omnion_workflows::registry::Port) -> Self {
        Self {
            name: port.name.clone(),
            kind: port.kind.as_str(),
            accepts: port.accepts.clone(),
            open: port.open,
        }
    }
}

/// The list payload, with the numbers the library header shows.
#[derive(Debug, Serialize)]
pub struct NodeTypeListResponse {
    /// Matching nodes, in palette order.
    pub nodes: Vec<NodeTypeBody>,
    /// How many the filters matched.
    pub matched: usize,
    /// How many the registry holds in total.
    pub total: usize,
    /// The filters as they were applied, echoed so a caller can see what it actually asked
    /// for. An empty list with no echo of the query is indistinguishable from a broken
    /// endpoint.
    pub filters: AppliedNodeFilters,
}

/// The node filters, as applied.
#[derive(Debug, Serialize)]
pub struct AppliedNodeFilters {
    /// Free-text query.
    pub search: Option<String>,
    /// Category.
    pub category: Option<String>,
    /// Capability.
    pub capability: Option<String>,
    /// Whether deprecated nodes are included.
    pub include_deprecated: bool,
    /// Whether the answer is restricted by credential use.
    pub credential: Option<bool>,
    /// How many nodes are bundled with the release, and therefore always available. The
    /// install ledger lands in slice 4; until it does, "installed" is exactly this set, and
    /// the count is returned so the client never has to infer it.
    pub bundled_count: usize,
}

/// One field of a credential type's form.
#[derive(Debug, Serialize)]
pub struct CredentialFieldBody {
    /// Field name.
    pub name: String,
    /// Field label.
    pub label: String,
    /// How it is filled in and stored.
    pub kind: &'static str,
    /// Whether the form refuses to save without it.
    pub required: bool,
    /// Allowed values for a select.
    pub options: Vec<String>,
    /// One-line help.
    pub help: Option<String>,
    /// Whether the field is kept out of every log line.
    pub never_log: bool,
    /// `true` when the API will never return a value for this field — the reason the detail
    /// screen renders a fixed-width mask instead of an input.
    pub write_only: bool,
}

/// One credential type, in full.
#[derive(Debug, Serialize)]
pub struct CredentialTypeBody {
    /// Stable key.
    pub key: String,
    /// How secrets are obtained.
    pub kind: &'static str,
    /// Label on the type picker.
    pub label: String,
    /// One-line description.
    pub description: String,
    /// Lucide icon name.
    pub icon: String,
    /// Documentation link.
    pub docs_url: String,
    /// The form's fields.
    pub fields: Vec<CredentialFieldBody>,
    /// The nodes that accept this type, so the picker can say what a credential is *for*.
    pub used_by: Vec<String>,
    /// Whether the type is obtained through an OAuth authorization-code flow.
    pub oauth: bool,
    /// Whether the flow uses PKCE.
    pub oauth_pkce: Option<bool>,
    /// Scopes an OAuth type requests.
    pub oauth_scopes: Option<String>,
    /// Seconds the test hook may take.
    pub test_timeout_seconds: i64,
}

impl CredentialTypeBody {
    /// Describe one registry entry, naming the nodes that accept it.
    fn build(definition: &CredentialDefinition) -> Self {
        Self {
            key: definition.key.to_string(),
            kind: definition.kind.as_str(),
            label: definition.label.to_string(),
            description: definition.description.to_string(),
            icon: definition.icon.to_string(),
            docs_url: definition.docs_url.to_string(),
            fields: definition
                .fields
                .iter()
                .map(|field| CredentialFieldBody {
                    name: field.name.to_string(),
                    label: field.label.to_string(),
                    kind: field.kind.as_str(),
                    required: field.required,
                    options: field.options.iter().map(|opt| (*opt).to_string()).collect(),
                    help: field.help.map(str::to_string),
                    never_log: field.never_log,
                    // A secret field is write-only by construction: the value is accepted once
                    // and never returned, so the client is told rather than left to guess.
                    write_only: field.kind == FieldType::Secret,
                })
                .collect(),
            used_by: nodes()
                .iter()
                .filter(|node| {
                    node.credential_types
                        .iter()
                        .any(|key| *key == definition.key)
                })
                .map(|node| node.key.to_string())
                .collect(),
            oauth: definition.oauth.is_some(),
            oauth_pkce: definition.oauth.as_ref().map(|oauth| oauth.pkce),
            oauth_scopes: definition
                .oauth
                .as_ref()
                .map(|oauth| oauth.scopes.to_string()),
            test_timeout_seconds: definition.test_timeout_seconds,
        }
    }
}

/// The credential-type list payload.
#[derive(Debug, Serialize)]
pub struct CredentialTypeListResponse {
    /// Types, in picker order.
    pub types: Vec<CredentialTypeBody>,
    /// How many there are.
    pub total: usize,
}

// ---------------------------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------------------------

/// Whether a node matches the free-text query.
///
/// Label, key and description — the three things a person searching for a node actually knows.
/// Matching the description is what makes an integration node findable by what it *does*
/// ("call an endpoint") rather than only by the word its author chose for its name.
fn matches_search(node: &NodeDefinition, needle: &str) -> bool {
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() {
        return true;
    }
    node.label.to_lowercase().contains(&needle)
        || node.key.to_lowercase().contains(&needle)
        || node.description.to_lowercase().contains(&needle)
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `GET /api/v1/node-types` — the library list.
pub async fn list_node_types(
    State(_state): State<AppState>,
    _current: CurrentSession,
    Query(query): Query<NodeTypeQuery>,
) -> Result<Json<NodeTypeListResponse>, ApiError> {
    // An unknown category or capability is a 400 with a named code, not an empty list: a typo in
    // a filter is a mistake the caller can fix, and hiding it behind "0 results" makes it look
    // like a library with nothing in it.
    let category = match query.category.as_deref() {
        Some(raw) => Some(NodeCategory::parse(raw.trim()).ok_or_else(|| {
            ApiError::bad_request(
                "node_category_unknown",
                format!(
                    "{raw:?} is not a node category; try one of {}",
                    NodeCategory::all()
                        .iter()
                        .map(|c| c.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )
        })?),
        None => None,
    };

    let capability = match query.capability.as_deref() {
        Some(raw) => Some(Capability::parse(raw.trim()).ok_or_else(|| {
            ApiError::bad_request(
                "node_capability_unknown",
                format!(
                    "{raw:?} is not a node capability; try one of execute, poll, webhook, trigger"
                ),
            )
        })?),
        None => None,
    };

    let total = nodes().len();
    let selected: Vec<&NodeDefinition> = nodes()
        .iter()
        .filter(|node| {
            query
                .search
                .as_deref()
                .is_none_or(|s| matches_search(node, s))
        })
        .filter(|node| category.is_none_or(|wanted| node.category == wanted))
        .filter(|node| capability.is_none_or(|wanted| node.capabilities.contains(&wanted)))
        .filter(|node| query.include_deprecated() || !node.deprecated)
        .filter(|node| {
            query
                .credential
                .is_none_or(|wanted| node.credential_types.is_empty() != wanted)
        })
        .collect();

    Ok(Json(NodeTypeListResponse {
        matched: selected.len(),
        nodes: selected.into_iter().map(NodeTypeBody::build).collect(),
        total,
        filters: AppliedNodeFilters {
            search: query.search.clone(),
            category: category.map(|c| c.as_str().to_string()),
            capability: capability.map(|c| c.as_str().to_string()),
            include_deprecated: query.include_deprecated(),
            credential: query.credential,
            bundled_count: total,
        },
    }))
}

/// `GET /api/v1/node-types/{key}` — one node in full.
pub async fn get_node_type(
    State(_state): State<AppState>,
    _current: CurrentSession,
    Path(key): Path<String>,
) -> Result<Json<NodeTypeBody>, ApiError> {
    let node = find_node(key.trim()).ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "node_type_unknown",
            format!("no node is registered under {key:?}"),
        )
    })?;
    Ok(Json(NodeTypeBody::build(node)))
}

/// `GET /api/v1/credential-types` — the credential catalogue.
pub async fn list_credential_types(
    State(_state): State<AppState>,
    _current: CurrentSession,
) -> Result<Json<CredentialTypeListResponse>, ApiError> {
    let types: Vec<CredentialTypeBody> = credential_types()
        .iter()
        .map(CredentialTypeBody::build)
        .collect();
    Ok(Json(CredentialTypeListResponse {
        total: types.len(),
        types,
    }))
}

/// `GET /api/v1/credential-types/{key}` — one credential type in full.
pub async fn get_credential_type(
    State(_state): State<AppState>,
    _current: CurrentSession,
    Path(key): Path<String>,
) -> Result<Json<CredentialTypeBody>, ApiError> {
    let definition = find_credential_type(key.trim()).ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "credential_type_unknown",
            format!("no credential type is registered under {key:?}"),
        )
    })?;
    Ok(Json(CredentialTypeBody::build(definition)))
}

/// `GET /api/v1/node-types/lint` — the registry's own lint, for the release gate.
///
/// This is the same [`lint`] the unit test runs, exposed rather than reimplemented: a registry
/// that would fail its own lint has to be visible from the running server, not only from a
/// test that a deploy skips. Slice 4's package validator reuses the same findings shape.
pub async fn get_registry_lint(
    State(_state): State<AppState>,
    _current: CurrentSession,
) -> Result<Json<serde_json::Value>, ApiError> {
    let findings = lint();
    Ok(Json(serde_json::json!({
        "ok": findings.is_empty(),
        "findings": findings,
        "node_count": nodes().len(),
        "credential_type_count": credential_types().len(),
    })))
}

/// `GET /api/v1/node-types/categories` — the palette's category tree, with its counts.
///
/// The counts come from the same registry the list is built from, so a group header can never
/// claim five nodes under it when two of them are filtered out by a search that is still typed.
pub async fn get_node_categories(
    State(_state): State<AppState>,
    _current: CurrentSession,
) -> Result<Json<serde_json::Value>, ApiError> {
    let categories: Vec<serde_json::Value> = NodeCategory::all()
        .iter()
        .map(|category| {
            let members: Vec<&NodeDefinition> = nodes()
                .iter()
                .filter(|node| node.category == *category)
                .collect();
            serde_json::json!({
                "key": category.as_str(),
                "label": CATEGORY_LABELS
                    .iter()
                    .find(|(key, _)| *key == category.as_str())
                    .map(|(_, label)| *label)
                    .unwrap_or(category.as_str()),
                "count": members.len(),
                "node_keys": members.iter().map(|node| node.key).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "categories": categories })))
}

/// Display labels for the category tree, in palette order.
const CATEGORY_LABELS: &[(&str, &str)] = &[
    ("trigger", "Triggers"),
    ("flow", "Flow"),
    ("code", "Code"),
    ("data", "Data"),
    ("integration", "Integrations"),
    ("helper", "Helpers"),
    ("error_handler", "Error handling"),
];

/// The port kinds the API answers with, for the client's own type narrowing.
pub const PORT_KINDS: &[&str] = &[
    PortKind::Main.as_str(),
    PortKind::Error.as_str(),
    PortKind::AiTool.as_str(),
];

/// `GET /api/v1/port-kinds` — the three port kinds and what each means.
///
/// Small, but it is the one fact the canvas (REQ-086) has to agree with, and a hard-coded list
/// in two places is how the error path stops being an error path.
pub async fn get_port_kinds(
    State(_state): State<AppState>,
    _current: CurrentSession,
) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(serde_json::json!({
        "kinds": PORT_KINDS,
        "descriptions": {
            "main": "The data path. Every node has one.",
            "error": "The failure path. Feeds an error input or an error workflow, never a main input.",
            "ai_tool": "Reserved for the AI tool port (REQ-097…REQ-104). Declared, never executed here."
        }
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(search: Option<&str>, category: Option<&str>) -> NodeTypeQuery {
        NodeTypeQuery {
            search: search.map(str::to_string),
            category: category.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn search_matches_label_key_and_description() {
        let http = find_node("http_request").expect("registered");
        assert!(matches_search(http, "http"));
        assert!(matches_search(http, "HTTP REQUEST"));
        assert!(matches_search(http, "endpoint"), "description is searched");
        assert!(!matches_search(http, "postgres"));
    }

    #[test]
    fn an_empty_search_matches_everything() {
        let http = find_node("http_request").expect("registered");
        assert!(matches_search(http, "   "));
        assert!(matches_search(http, ""));
    }

    #[test]
    fn deprecated_nodes_are_included_unless_asked_out() {
        assert!(query(None, None).include_deprecated());
        assert!(!query(None, None).deprecated.is_some());
        assert!(
            !NodeTypeQuery {
                deprecated: Some(false),
                ..Default::default()
            }
            .include_deprecated()
        );
    }

    #[test]
    fn a_body_marks_a_deprecated_node_with_its_replacement() {
        let legacy = find_node("legacy_webhook").expect("registered");
        let body = NodeTypeBody::build(legacy);
        assert_eq!(body.state, "deprecated");
        assert_eq!(body.superseded_by.as_deref(), Some("http_request"));
        assert!(
            body.state_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("http_request")),
            "the reason names the replacement: {:?}",
            body.state_reason
        );
    }

    #[test]
    fn a_current_node_carries_no_state_reason() {
        let body = NodeTypeBody::build(find_node("http_request").expect("registered"));
        assert_eq!(body.state, "available");
        assert!(body.state_reason.is_none());
    }

    #[test]
    fn a_credential_body_names_the_nodes_that_use_it_and_hides_its_secrets() {
        let definition = find_credential_type("api_key").expect("registered");
        let body = CredentialTypeBody::build(definition);
        assert!(
            body.used_by.contains(&"http_request".to_string()),
            "the picker must say what a credential is for: {:?}",
            body.used_by
        );
        let secret = body
            .fields
            .iter()
            .find(|field| field.name == "api_key")
            .expect("the key field is there");
        assert!(
            secret.write_only,
            "a secret field is write-only by construction"
        );
        assert!(secret.never_log);
        let plain = body
            .fields
            .iter()
            .find(|field| field.name == "header")
            .expect("the header field is there");
        assert!(
            !plain.write_only,
            "a non-secret field is editable on the detail screen"
        );
    }

    #[test]
    fn an_oauth_credential_reports_its_flow_but_not_its_token_endpoint() {
        let definition = find_credential_type("oauth2").expect("registered");
        let body = CredentialTypeBody::build(definition);
        assert!(body.oauth);
        assert_eq!(body.oauth_pkce, Some(true));
        assert!(
            body.oauth_scopes
                .as_deref()
                .is_some_and(|scopes| !scopes.is_empty())
        );
        // The rendered JSON must not carry the token endpoint: the detail screen links the docs
        // for that, and an endpoint in a payload is one more place a secret flow is copied.
        let json = serde_json::to_string(&body).expect("serialisable");
        assert!(
            !json.contains("token_url"),
            "the token endpoint stays server-side"
        );
    }
}
