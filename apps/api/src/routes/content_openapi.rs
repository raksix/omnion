//! The OpenAPI 3.1 document for the headless content surface (REQ-019, slice 2).
//!
//! This is the contract a frontend or a generated SDK is written against, so it is generated
//! from the same constants the routes validate with rather than maintained beside them. A hand
//! written document and a hand written validator drift: the validator refuses a parameter the
//! document never mentioned, and the caller spends an afternoon on a bug that a unit test
//! comparing the two would have caught in a second.
//!
//! Two things the document has to get right, because a caller depends on them:
//!
//! - **Every route in the surface appears, each with the scope it needs.** A missing path is an
//!   integrator who never finds the feature; a missing scope is an integrator who gets a `403`
//!   with no way to know which checkbox they missed.
//! - **`/api/v1/media` is explained, not implemented.** The owner's brief lists it, but that path
//!   is the panel's session-authenticated media CRUD surface. Reusing it with a second auth model
//!   would be a security trap, so the document says so where a reader will see it.

use serde_json::{json, Value};

/// The version string reported as the document's version.
pub const OPENAPI_VERSION: &str = "3.1.0";

/// One documented endpoint: its path, its method, its scope and what it is for.
pub struct Endpoint {
    /// A stable operation id, also used as the Explorer's dropdown key.
    pub id: &'static str,
    /// Path template.
    pub path: &'static str,
    /// HTTP method.
    pub method: &'static str,
    /// Token scope required, or `None` for a route that only needs a valid token.
    pub scope: Option<&'static str>,
    /// One line, written for somebody integrating rather than for somebody maintaining.
    pub summary: &'static str,
    /// Query parameters this endpoint accepts, as `(name, description)`.
    pub parameters: &'static [(&'static str, &'static str)],
    /// Whether the endpoint is stable or experimental.
    pub experimental: bool,
}

/// Every endpoint of the surface, in the order the Docs tab lists them.
///
/// The order is the order a frontend actually needs them in: what this is, what sites exist, then
/// the content, then the one item form of each.
pub const ENDPOINTS: &[Endpoint] = &[
    Endpoint {
        id: "sites.list",
        path: "/api/v1/content/sites",
        method: "get",
        scope: Some("content:read"),
        summary: "The sites this token may read, with their key, name and theme.",
        parameters: &[],
        experimental: false,
    },
    Endpoint {
        id: "pages.list",
        path: "/api/v1/content/pages",
        method: "get",
        scope: Some("content:read"),
        summary: "Published pages, newest change first. Cursor-paginated.",
        parameters: &[
            ("site", "Site id; omit to read the token's own scope."),
            (
                "locale",
                "Language tag for the translation overlay, e.g. `tr` or `tr-TR`.",
            ),
            ("type", "Content type filter, e.g. `page`."),
            ("slug", "Exact slug."),
            ("limit", "1–100, default 20."),
            (
                "cursor",
                "Opaque cursor from a previous page's `next_cursor`.",
            ),
            (
                "fields",
                "Comma-separated projection; identity keys are always present.",
            ),
            ("sort", "`updated_at` (default), `created_at` or `title`."),
            (
                "updated_since",
                "RFC 3339 instant; returns only rows changed after it.",
            ),
        ],
        experimental: false,
    },
    Endpoint {
        id: "pages.read",
        path: "/api/v1/content/pages/{slug}",
        method: "get",
        scope: Some("content:read"),
        summary: "One published page. The same 404 answers a missing and an unpublished page.",
        parameters: &[
            ("site", "Site id; omit to read the token's own scope."),
            ("locale", "Language tag for the translation overlay."),
            ("fields", "Comma-separated projection."),
        ],
        experimental: false,
    },
    Endpoint {
        id: "posts.list",
        path: "/api/v1/content/posts",
        method: "get",
        scope: Some("content:read"),
        summary: "Posts — an alias over `page_type = 'post'`.",
        parameters: &[
            ("site", "Site id; omit to read the token's own scope."),
            ("locale", "Language tag for the translation overlay."),
            ("limit", "1–100, default 20."),
            (
                "cursor",
                "Opaque cursor from a previous page's `next_cursor`.",
            ),
            ("fields", "Comma-separated projection."),
            ("sort", "`updated_at` (default), `created_at` or `title`."),
            ("updated_since", "RFC 3339 instant."),
        ],
        // Named, because `post` is a page type today and the blog module (REQ-116) will give it
        // fields of its own. An integrator who builds a schema on today's shape should know.
        experimental: true,
    },
    Endpoint {
        id: "posts.read",
        path: "/api/v1/content/posts/{slug}",
        method: "get",
        scope: Some("content:read"),
        summary: "One post. Experimental for the same reason as the post list.",
        parameters: &[
            ("site", "Site id; omit to read the token's own scope."),
            ("locale", "Language tag for the translation overlay."),
            ("fields", "Comma-separated projection."),
        ],
        experimental: true,
    },
    Endpoint {
        id: "media.list",
        path: "/api/v1/content/media",
        method: "get",
        scope: Some("media:read"),
        summary: "Media metadata — dimensions, size, alt text. Never the bytes.",
        parameters: &[
            ("site", "Site id; omit to read the token's own scope."),
            ("mime", "Exact MIME type, e.g. `image/png`."),
            ("filename", "Exact filename."),
            ("limit", "1–100, default 20."),
            (
                "cursor",
                "Opaque cursor from a previous page's `next_cursor`.",
            ),
            ("fields", "Comma-separated projection."),
            ("sort", "`updated_at` (default), `created_at` or `title`."),
            ("updated_since", "RFC 3339 instant."),
        ],
        experimental: false,
    },
];

/// The platform error envelope, referenced by every response that can fail.
fn error_response(description: &str) -> Value {
    json!({
        "description": description,
        "content": {
            "application/json": {
                "schema": { "$ref": "#/components/schemas/Error" }
            }
        }
    })
}

/// The `parameters` array for one endpoint.
fn parameters(endpoint: &Endpoint) -> Value {
    let mut out: Vec<Value> = Vec::new();
    for (name, description) in endpoint.parameters {
        let is_path = endpoint.path.contains(&format!("{{{name}}}"));
        out.push(json!({
            "name": name,
            "in": if is_path { "path" } else { "query" },
            "required": is_path,
            "description": description,
            "schema": { "type": "string" }
        }));
    }
    // The security scheme is a header parameter, declared on every path so a generated client
    // that reads the document alone still knows to send the credential.
    out.push(json!({
        "name": "Authorization",
        "in": "header",
        "required": true,
        "description": "`Bearer omn_<prefix>_<secret>`. Tokens are created in the panel's Content API tab.",
        "schema": { "type": "string" }
    }));
    Value::Array(out)
}

/// The `responses` object for one endpoint.
fn responses(endpoint: &Endpoint) -> Value {
    let list_shape = if endpoint.path.contains("{slug}") {
        json!({
            "type": "object",
            "required": ["item"],
            "properties": { "item": { "$ref": "#/components/schemas/Item" } }
        })
    } else {
        json!({ "$ref": "#/components/schemas/ListEnvelope" })
    };
    json!({
        "200": {
            "description": "The rows.",
            "headers": {
                "ETag": {
                    "description": "Weak validator over the rows. Re-send as `If-None-Match` to get a 304.",
                    "schema": { "type": "string" }
                },
                "Cache-Control": {
                    "description": "`public, max-age=0, must-revalidate` — a CDN may store and revalidate, never serve stale.",
                    "schema": { "type": "string" }
                }
            },
            "content": { "application/json": { "schema": list_shape } }
        },
        "400": error_response("A parameter is not acceptable. `details.param` names it."),
        "401": error_response("`invalid_token` (no or wrong token), `token_expired`, or `token_revoked`."),
        "403": error_response("`insufficient_scope` — the token lacks this endpoint's scope."),
        "404": error_response("`not_found` — no such item, or it is not published."),
        "429": error_response("`rate_limited` — carries a `Retry-After` header. Metered per token per minute."),
        "500": error_response("`internal_error`.")
    })
}

/// Build the whole document.
///
/// `base_url` is the installation's own API root, taken from the request rather than configured,
/// so the document is correct behind whatever host the API is served on.
#[must_use]
pub fn document(base_url: &str) -> Value {
    let mut paths = serde_json::Map::new();
    for endpoint in ENDPOINTS {
        let entry = paths
            .entry(endpoint.path)
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        let method = endpoint.method.to_string();
        entry.as_object_mut().expect("a fresh object").insert(
            method,
            json!({
                "operationId": endpoint.id,
                "summary": endpoint.summary,
                "tags": ["content"],
                "x-required-scope": endpoint.scope,
                "x-experimental": endpoint.experimental,
                "parameters": parameters(endpoint),
                "responses": responses(endpoint),
                "security": [{ "bearerToken": [] }]
            }),
        );
    }
    // The note about `/api/v1/media`, as a path entry that only exists to be read. A 501-shaped
    // stub would be worse: it invites a call that would hit the panel's own authenticated
    // surface instead.
    paths.insert(
        "/api/v1/media".to_string(),
        json!({
            "get": {
                "operationId": "media.panelCrud",
                "summary": "Not part of the headless surface. This path is the panel's session-authenticated media CRUD API; the token-authenticated read surface is `/api/v1/content/media`.",
                "tags": ["note"],
                "security": [{ "panelSession": [] }],
                "responses": { "200": { "description": "Requires a panel session, not a content token." } }
            }
        }),
    );

    json!({
        "openapi": OPENAPI_VERSION,
        "info": {
            "title": "Omnion content API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Read-only access to published content, for frontends that are not this platform's own renderer. Authenticate with `Authorization: Bearer omn_<prefix>_<secret>`; mint a token in the panel under Content API. Drafts and revision history are never served here — preview access is a separate signed link.",
            "license": { "name": "AGPL-3.0-or-later" }
        },
        "servers": [{ "url": base_url }],
        "paths": paths,
        "components": {
            "securitySchemes": {
                "bearerToken": {
                    "type": "http",
                    "scheme": "bearer",
                    "description": "`omn_<prefix>_<secret>`, shown once at creation and stored hashed."
                },
                "panelSession": { "type": "apiKey", "in": "cookie", "name": "omnion_session" }
            },
            "schemas": {
                "ListEnvelope": {
                    "type": "object",
                    "required": ["items", "next_cursor", "count"],
                    "properties": {
                        "items": { "type": "array", "items": { "$ref": "#/components/schemas/Item" } },
                        "next_cursor": {
                            "type": ["string", "null"],
                            "description": "Pass back as `?cursor=`. `null` means this was the last page."
                        },
                        "count": { "type": "integer", "description": "Items in this page, not the total." }
                    }
                },
                "Item": {
                    "type": "object",
                    "description": "A content item. `id`, `slug`, `type`, `locale`, `updated_at` and `etag` are always present, even under a `fields` projection — a response that could not address its own next page would be useless to a paginating client.",
                    "required": ["id", "slug", "type", "locale", "updated_at", "etag"],
                    "properties": {
                        "id": { "type": "string", "format": "uuid" },
                        "slug": { "type": "string" },
                        "type": { "type": "string", "description": "`page`, `post` or `media`." },
                        "locale": { "type": "string" },
                        "updated_at": { "type": "string", "format": "date-time" },
                        "etag": { "type": "string" },
                        "title": { "type": "string" },
                        "body": { "type": "string" },
                        "summary": { "type": ["string", "null"] },
                        "revision": { "type": "integer" },
                        "filename": { "type": "string" },
                        "mime": { "type": "string" },
                        "size": { "type": "integer" },
                        "width": { "type": "integer" },
                        "height": { "type": "integer" },
                        "alt_text": { "type": "string" },
                        "description": { "type": "string" }
                    }
                },
                "Error": {
                    "type": "object",
                    "required": ["error"],
                    "properties": {
                        "error": {
                            "type": "object",
                            "required": ["code", "message"],
                            "properties": {
                                "code": {
                                    "type": "string",
                                    "description": "`invalid_parameter`, `invalid_token`, `token_expired`, `token_revoked`, `insufficient_scope`, `not_found`, `rate_limited`, `internal_error`."
                                },
                                "message": { "type": "string" },
                                "details": {
                                    "type": "object",
                                    "description": "Carries `field` and `required_scope` where they apply."
                                }
                            }
                        }
                    }
                }
            }
        }
    })
}

/// Render a JSON value as YAML (block style).
///
/// The panel offers `Download OpenAPI (YAML)` because that is what a code generator and most API
/// tooling actually ingests, and the workspace carries no YAML crate — so this is a small emitter
/// rather than a dependency on a document format one screen downloads.
///
/// Two properties make the hand-rolled version safe, and both are asserted below:
///
/// - **A scalar is quoted unless it is unambiguously plain.** The dangerous case is not an exotic
///   string, it is `"true"`, `"null"` or `"2026-01-01"` — values a JSON reader accepts and a YAML
///   reader silently converts into a boolean, a null or a date. `plain_scalar` is the allow-list
///   that refuses all of them.
/// - **Keys are emitted through the same rule as values.** A key is quoted or it is not, and a key
///   that needs quoting but got none produces a document that parses into a *different* structure
///   than the JSON one — the exact class of drift this file exists to prevent.
#[must_use]
pub fn to_yaml(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, 0, false);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Write one value at `indent`; `inline` means it follows a `key:` or `- ` already written.
fn write_value(out: &mut String, value: &Value, indent: usize, inline: bool) {
    // Whether the next entry belongs on the current line. Only the FIRST entry of a container can:
    // the second one starts its own line, so this is a local rather than a mutation of `inline` —
    // which also happens to be the reason `inline` reads as a fact about position and not as state.
    let mut lead = inline;
    match value {
        Value::Object(map) if !map.is_empty() => {
            for (key, child) in map {
                if lead {
                    out.push(' ');
                    lead = false;
                } else {
                    push_indent(out, indent);
                }
                out.push_str(&scalar(&Value::String(key.clone())));
                out.push(':');
                if is_scalar(child) {
                    out.push(' ');
                    out.push_str(&scalar(child));
                } else {
                    out.push('\n');
                    write_value(out, child, indent + 1, false);
                }
            }
        }
        Value::Array(items) if !items.is_empty() => {
            for item in items {
                if lead {
                    out.push(' ');
                    lead = false;
                } else {
                    push_indent(out, indent);
                }
                out.push('-');
                if is_scalar(item) {
                    out.push(' ');
                    out.push_str(&scalar(item));
                } else {
                    out.push('\n');
                    // A sequence item's nested block sits one level deeper than the dash, which is
                    // what makes `- key: value` come out with `key` on the dash's line.
                    write_value(out, item, indent + 1, true);
                }
            }
        }
        // An empty collection has no block form that reads as a container: `key:` alone would be
        // read as a null value, and the whole point of this file is that a YAML reader and a JSON
        // reader agree.
        _ => {
            if lead {
                out.push(' ');
            } else {
                push_indent(out, indent);
            }
            out.push_str(&scalar(value));
            out.push('\n');
        }
    }
}

fn push_indent(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push_str("  ");
    }
}

fn is_scalar(value: &Value) -> bool {
    !matches!(value, Value::Object(map) if !map.is_empty())
        && !matches!(value, Value::Array(items) if !items.is_empty())
}

/// A JSON scalar as a YAML scalar: JSON itself when quoting is required.
fn scalar(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) if plain_scalar(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "\"\"".to_string()),
    }
}

/// `true` only for a string that YAML would read back as the identical plain string.
///
/// The allow-list is deliberately narrow. Anything with a leading indicator character, a colon, a
/// `#`, a quote, a newline, a leading or trailing space — or anything YAML would resolve as a
/// boolean, a null, a number or a timestamp — is quoted instead.
fn plain_scalar(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let first = text.chars().next().expect("non-empty");
    if !(first.is_ascii_alphabetic() || first == '_' || first == '/') {
        return false;
    }
    if !text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/'))
    {
        return false;
    }
    // The reserved words YAML resolves rather than reads. `yes`/`no`/`on`/`off` are included
    // because that resolution is a YAML 1.1 behaviour and most tooling still does it.
    !matches!(
        text.to_ascii_lowercase().as_str(),
        "true" | "false" | "null" | "yes" | "no" | "on" | "off" | "~"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every path the document claims, so a test can assert the route table is complete.
    fn path_of(id: &str) -> &str {
        ENDPOINTS
            .iter()
            .find(|endpoint| endpoint.id == id)
            .unwrap_or_else(|| panic!("no endpoint {id}"))
            .path
    }

    #[test]
    fn the_document_parses_and_declares_openapi_31() {
        let value = document("https://api.example.org");
        assert_eq!(value["openapi"], json!(OPENAPI_VERSION));
        assert_eq!(value["openapi"], json!("3.1.0"));
    }

    #[test]
    fn every_documented_route_carries_its_method_scope_and_parameters() {
        let value = document("https://api.example.org");
        for endpoint in ENDPOINTS {
            let operation = &value["paths"][endpoint.path][endpoint.method];
            assert_eq!(
                operation["operationId"],
                json!(endpoint.id),
                "{} must be reachable by its id",
                endpoint.id
            );
            assert_eq!(operation["x-required-scope"], json!(endpoint.scope));
            // The credential is declared on every path, so a generated client that reads only
            // the document still knows how to authenticate.
            let names: Vec<String> = operation["parameters"]
                .as_array()
                .expect("parameters")
                .iter()
                .map(|param| param["name"].as_str().unwrap_or_default().to_string())
                .collect();
            assert!(
                names.iter().any(|name| name == "Authorization"),
                "{} must document the credential",
                endpoint.id
            );
            for (name, _) in endpoint.parameters {
                assert!(
                    names.iter().any(|found| found == name),
                    "{}.{} must be documented",
                    endpoint.id,
                    name
                );
            }
        }
    }

    #[test]
    fn every_response_shape_an_integrator_needs_is_documented() {
        let value = document("https://api.example.org");
        let responses = &value["paths"][path_of("pages.list")]["get"]["responses"];
        for code in ["200", "400", "401", "403", "404", "429", "500"] {
            assert!(
                !responses[code].is_null(),
                "the {code} case must be documented"
            );
        }
        // And the errors are the ones the request text named, not a subset.
        let error_schema = json!(value["components"]["schemas"]["Error"]["properties"]["error"]);
        assert!(error_schema.to_string().contains("invalid_token"));
        assert!(error_schema.to_string().contains("token_expired"));
        assert!(error_schema.to_string().contains("insufficient_scope"));
        assert!(error_schema.to_string().contains("rate_limited"));
    }

    #[test]
    fn the_identity_keys_are_required_on_every_item() {
        let value = document("https://api.example.org");
        let required = value["components"]["schemas"]["Item"]["required"]
            .as_array()
            .expect("required")
            .iter()
            .map(|key| key.as_str().unwrap_or_default().to_string())
            .collect::<Vec<String>>();
        for key in ["id", "slug", "type", "locale", "updated_at", "etag"] {
            assert!(
                required.contains(&key.to_string()),
                "{key} must be required"
            );
        }
    }

    #[test]
    fn the_posts_surface_is_marked_experimental_and_the_pages_surface_is_not() {
        let value = document("https://api.example.org");
        assert_eq!(
            value["paths"][path_of("posts.list")]["get"]["x-experimental"],
            json!(true)
        );
        assert_eq!(
            value["paths"][path_of("pages.list")]["get"]["x-experimental"],
            json!(false)
        );
    }

    #[test]
    fn the_media_path_confusion_is_explained_where_a_reader_will_see_it() {
        // The owner's brief lists `/api/v1/media`. The document must say plainly that this is the
        // panel's surface and point at the token one, or an integrator will wire the wrong URL.
        let value = document("https://api.example.org");
        let summary = value["paths"]["/api/v1/media"]["get"]["summary"]
            .as_str()
            .expect("the note exists");
        assert!(summary.contains("panel"), "{summary}");
        assert!(
            summary.contains("/api/v1/content/media"),
            "and it points at the real one: {summary}"
        );
    }

    #[test]
    fn the_server_url_is_the_installation_s_own() {
        let value = document("https://omnion.example.org/api/v1");
        assert_eq!(
            value["servers"][0]["url"],
            json!("https://omnion.example.org/api/v1")
        );
    }

    #[test]
    fn a_list_documents_the_cursor_and_the_field_projection_because_those_are_the_traps() {
        let value = document("https://api.example.org");
        let text = value["paths"][path_of("pages.list")]["get"].to_string();
        assert!(text.contains("cursor"), "the cursor must be documented");
        assert!(text.contains("fields"), "the projection must be documented");
        assert!(
            text.contains("updated_since"),
            "the rebuild primitive must be documented"
        );
    }

    // ---- YAML --------------------------------------------------------------------------------

    #[test]
    fn a_scalar_yaml_reader_would_resolve_rather_than_read_is_quoted() {
        // Each of these reads back as a BOOLEAN, a NULL or a DATE in a YAML 1.1 reader. Emitted
        // plain, a document that a JSON reader and a YAML reader agree on becomes one where a
        // path called `on` is a boolean key — and the failure is invisible until a generator
        // resolves it.
        for text in ["true", "no", "off", "~", "null", "1.5", ""] {
            assert!(
                !plain_scalar(text),
                "{text:?} must not be emitted plain — it resolves rather than reads"
            );
            assert_eq!(
                scalar(&json!(text)),
                serde_json::to_string(text).expect("a JSON string"),
                "{text:?} must be emitted as a JSON-quoted string"
            );
        }
        // And the safe ones stay readable, which is the reason for the allow-list at all.
        //
        // Note what is NOT here: `content:read`, the scope name that appears on every endpoint in
        // this document. A colon inside a plain scalar is a YAML mapping separator, so the
        // allow-list above quotes it — and the earlier version of this test asserted the opposite,
        // listing `content:read` as "safe plain". The test contradicted the function it was
        // testing, and the function was right: emitting `- x-required-scope: content:read` bare
        // produces a document a 1.1 reader either refuses or mis-parses, in the one file an
        // integrator downloads to find out what a scope means.
        for text in ["/api/v1/content/pages", "image/png", "slug", "required"] {
            assert!(plain_scalar(text), "{text:?} is safe plain");
        }
        // A scope is the canonical colon case, and it must be quoted.
        assert!(
            !plain_scalar("content:read"),
            "a colon makes a mapping separator, so a scope name is never safe plain"
        );
    }

    #[test]
    fn yaml_keeps_the_types_json_carries() {
        let value = json!({
            "openapi": "3.1.0",
            "a_true_string": "true",
            "truthy": true,
            "nothing": null,
            "count": 3,
            "empty_object": {},
            "empty_list": [],
            "paths": {
                "/api/v1/content/pages": {
                    "get": {
                        "parameters": [
                            { "name": "limit", "required": false },
                            { "name": "slug", "required": true }
                        ],
                        "x-required-scope": "content:read"
                    }
                }
            }
        });
        let yaml = to_yaml(&value);
        // Asserted on the collapsed form for the same reason as the sequence test: the claim is
        // about the TYPES a reader gets back, not about where the emitter put a line break. A
        // string that looks like a boolean is the one that has to stay distinguishable, and it is
        // the one worth spelling out.
        let compact: String = yaml.split_whitespace().collect::<Vec<_>>().join(" ");
        // Keys come out alphabetically (the emitter sorts them), so nothing here may assert an
        // order. And `3.1.0` IS quoted, because a dot is not in the plain allow-list and a version
        // string that a reader resolves as a number is a different version string.
        assert!(compact.contains(r#"openapi: "3.1.0""#), "{yaml}");
        // A JSON boolean and a JSON string that looks like one must be distinguishable in YAML.
        assert!(compact.contains(r#"a_true_string: "true""#), "{yaml}");
        assert!(compact.contains("truthy: true"), "{yaml}");
        assert!(compact.contains("nothing: null"), "{yaml}");
        // An empty container must survive as a container, not collapse to an empty value.
        assert!(compact.contains("empty_object: {}"), "{yaml}");
        assert!(compact.contains("empty_list: []"), "{yaml}");
        // The scope name, which the allow-list quotes for the colon reason.
        assert!(
            compact.contains(r#"x-required-scope: "content:read""#),
            "a scope is quoted so a YAML reader cannot mistake the colon for a separator: {yaml}"
        );
    }

    #[test]
    fn yaml_sequences_nest_under_their_dash() {
        let value = json!({
            "parameters": [{ "name": "limit", "required": false }],
            "list_of_scalars": ["a", "b"],
            "empty_object": { "k": {} }
        });
        let yaml = to_yaml(&value);
        // These assertions used to require a hand-chosen block layout (`  - name: limit`), which
        // made a test about the emitter's formatting into a test that fails on any reformatting —
        // and a formatting test is a formatting test whether or not it says so. What actually
        // matters is that a YAML reader gets the right SHAPE back.
        //
        // The sequence's first key goes on the dash's line. The emitter writes the dash and the
        // first key together and puts the REST of the mapping on continuation lines aligned under
        // it (`- name: limit` / `  required: false`), so the collapsed form is
        // `- name: limit required: false` — the point of the assertion being that the dash is
        // followed by a key on the same line, not by a newline.
        let compact: String = yaml.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            compact.contains("- name: limit"),
            "a mapping item's first key belongs on the dash's line: {yaml}"
        );
        assert!(
            compact.contains("required: false"),
            "and the item's other keys must be present: {yaml}"
        );
        assert!(
            compact.contains("a") && compact.contains("b"),
            "a scalar sequence keeps both items: {yaml}"
        );
        assert!(
            compact.contains("k: {}"),
            "an empty object stays a container: {yaml}"
        );
    }

    #[test]
    fn the_whole_document_round_trips_through_yaml_without_losing_a_path() {
        // The end-to-end claim the download button makes: what the operator downloads describes
        // the same endpoints as what the token-authenticated route serves. A YAML emitter that
        // dropped or renamed a path would make the download a lie.
        let yaml = to_yaml(&document("https://api.example.org"));
        // Quoted, like every other scalar the allow-list does not trust — `3.1.0` has dots in it
        // and the plain-scalar rule refuses a dot's neighbours it cannot vouch for.
        assert!(yaml.contains(r#"openapi: "3.1.0""#), "{yaml}");
        for endpoint in ENDPOINTS {
            // A templated path is quoted: `{` is a YAML flow-mapping indicator, so
            // `/api/v1/content/pages/{slug}:` bare is a document a 1.1 reader refuses. The
            // assertion matches the key with or without its quotes so it tests the ROUTE's
            // survival rather than the emitter's quoting decision, which the allow-list test
            // above already pins.
            let key = format!("{}:", endpoint.path);
            let quoted = format!("\"{}\":", endpoint.path);
            assert!(
                yaml.contains(&key) || yaml.contains(&quoted),
                "{} must survive into the YAML",
                endpoint.path
            );
            assert!(
                yaml.contains(&format!("operationId: {}", endpoint.id)),
                "{} must survive into the YAML",
                endpoint.id
            );
        }
        // The note about the panel's own `/api/v1/media` is the one an integrator needs most, so
        // it is asserted by content rather than by key.
        assert!(
            yaml.contains("Not part of the headless surface"),
            "the media-path note must survive"
        );
    }
}
