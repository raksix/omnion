//! SDK generation from the emitted OpenAPI document.
//!
//! **The document is the only input.** Nothing here reads the router, the database or a fixture:
//! if the generators could see a route the document does not describe, a client could be
//! published for an endpoint that is not installed, and the drift gate in [`crate::openapi`]
//! would have nothing to say about it. So a generator takes the committed snapshot, and the
//! pinned hash is the *input's* identity rather than a value it computed afterwards.
//!
//! ## Why generation is in Rust at all
//!
//! Two languages' worth of output would be a second templating language with its own escaping
//! rules, and a Python heredoc writing TypeScript is a place where a quote in a route summary
//! becomes a broken package. Here a summary is a `String` on the way into a `format!`, and the
//! escaping is a function with tests.
//!
//! ## What a generated client deliberately does NOT contain
//!
//! The request's risk note says SDKs are public artifacts and their content must carry no
//! environment values, hostnames or example tokens. That is a property of the *emitter*, so it
//! is asserted structurally by [`scan_for_secrets`] and by a test that runs it over generated
//! output rather than trusting the reviewer's eye.

use crate::error::{Code, Error, Result};

/// A document defect, in the crate's own error shape.
///
/// [`Error`] is a set of struct variants carrying the code and the data a client acts on, so a
/// new caller does not get to invent a constructor — it picks the variant whose `extensions` it
/// would want a client to receive. A document that does not parse is `GRAPHQL_VALIDATION_FAILED`
/// here for a deliberately boring reason: the generator is a *build* step, and its errors go to
/// whoever ran the build, not to an HTTP client. What matters is that it refuses rather than
/// emitting a package from a document it could not fully read.
fn defect(message: impl Into<String>) -> Error {
    Error::Validation {
        code: Code::GraphqlValidationFailed,
        message: message.into(),
    }
}
use crate::openapi::{canonical_json, sanitize_identifier, Drift};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

/// The languages this module emits. Bounded on purpose: a third language is a third escaping
/// rule and a third thing to keep compiling, and the request ships two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Language {
    TypeScript,
    Python,
}

impl Language {
    /// The identifier used in release rows and file names.
    pub fn as_str(self) -> &'static str {
        match self {
            Language::TypeScript => "typescript",
            Language::Python => "python",
        }
    }

    /// Every language, in a fixed order so a release manifest is deterministic.
    pub fn all() -> [Language; 2] {
        [Language::TypeScript, Language::Python]
    }

    /// Parse the wire spelling, for `--language` on the CLI.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "typescript" | "ts" => Ok(Language::TypeScript),
            "python" | "py" => Ok(Language::Python),
            other => Err(defect(format!("`{other}` is not a language this platform ships; expected typescript or python"),
            )),
        }
    }
}

/// One operation, flattened out of the document.
///
/// Flattening happens once, here, so both emitters read the same struct. Two emitters each
/// walking `paths` would be two places to fix when a path parameter appears, and the difference
/// between them would be a client that compiles in one language and not the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    /// The document's operation id, already a legal identifier in both languages.
    pub id: String,
    /// Upper-case HTTP method.
    pub method: String,
    /// The axum path pattern, e.g. `/graphql/documents/{id}`.
    pub path: String,
    /// One-line summary from the annotation.
    pub summary: String,
    /// The first tag, which becomes the client method's group.
    pub tag: String,
    /// The catalogue key the guard enforces, empty when the route is genuinely unguarded.
    pub permission: String,
    /// Whether the annotation marks it deprecated.
    pub deprecated: bool,
    /// Path parameters in the order they appear in the pattern.
    pub parameters: Vec<String>,
}

impl Operation {
    /// The path with `{id}` replaced by a positional placeholder, and the parameters it consumed.
    ///
    /// Returns an error rather than guessing when the two disagree: a generator that silently
    /// drops an argument produces a client whose every call to that route returns `404`, and
    /// nothing in its own test suite would notice because the test uses the same broken path.
    pub fn path_template(&self) -> Result<(String, Vec<String>)> {
        let mut template = String::with_capacity(self.path.len());
        let mut consumed = Vec::new();
        let mut rest = self.path.as_str();
        while let Some(open) = rest.find('{') {
            template.push_str(&rest[..open]);
            let tail = &rest[open + 1..];
            let Some(close) = tail.find('}') else {
                return Err(defect(format!("`{}` has an unclosed path parameter", self.path),
                ));
            };
            let name = &tail[..close];
            if !self.parameters.contains(&name.to_string()) {
                return Err(defect(format!("`{}` declares {name} but the pattern does not use it", self.id),
                ));
            }
            template.push('{');
            template.push_str(&consumed.len().to_string());
            template.push('}');
            consumed.push(name.to_string());
            rest = &tail[close + 1..];
        }
        template.push_str(rest);
        for name in &self.parameters {
            if !consumed.contains(name) {
                return Err(defect(format!("`{}` declares {name} but the pattern does not use it", self.id),
                ));
            }
        }
        Ok((template, consumed))
    }
}

/// Read every operation out of a document, in a deterministic order.
///
/// The order is `(tag, id)` rather than the document's own — the document is already sorted by
/// path, and sorting by tag groups a client's methods the way a human reads them. Determinism
/// is the point: the same document must produce byte-identical output, or "generated from a
/// pinned hash" is a claim about a hash and nothing else.
pub fn operations(document: &Value) -> Result<Vec<Operation>> {
    const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];
    let Some(paths) = document.get("paths").and_then(Value::as_object) else {
        return Err(defect("the document has no `paths` object"));
    };

    let mut out = Vec::new();
    let mut seen_ids = BTreeSet::new();
    for (path, item) in paths {
        let Some(item) = item.as_object() else {
            return Err(defect(format!("`{path}` is not a path item object"),
            ));
        };
        let parameters = path
            .split('/')
            .filter_map(|s| s.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        for method in METHODS {
            let Some(op) = item.get(method) else { continue };
            let id = op
                .get("operationId")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    defect(format!("`{method} {path}` has no operationId, so a client cannot name it"),
                    )
                })?
                .to_string();
            if !seen_ids.insert(id.clone()) {
                return Err(defect(format!("`{id}` appears twice in the document; every generator rejects that"),
                ));
            }
            let tag = op
                .get("tags")
                .and_then(Value::as_array)
                .and_then(|tags| tags.first())
                .and_then(Value::as_str)
                .unwrap_or("general")
                .to_string();
            out.push(Operation {
                id,
                method: method.to_ascii_uppercase(),
                path: path.clone(),
                summary: op
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or("No summary.")
                    .to_string(),
                tag,
                permission: op
                    .get("x-omnion-permission")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                deprecated: op
                    .get("deprecated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                parameters: parameters.clone(),
            });
        }
    }
    out.sort_by(|a, b| a.tag.cmp(&b.tag).then_with(|| a.id.cmp(&b.id)));
    Ok(out)
}

/// One generated file plus the language it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedFile {
    /// Path inside the package, e.g. `src/index.ts`.
    pub path: String,
    pub contents: String,
}

/// A generated package: its files, the document it came from and that document's hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub language: Language,
    pub files: Vec<GeneratedFile>,
    /// The hash the caller pinned. Generation refuses to run without it, so a package can
    /// never claim a provenance it was not built from.
    pub openapi_hash: String,
    /// The API version the document declares, so the client's own version is the API's.
    pub api_version: String,
}

impl Package {
    /// Concatenate the files, for a content scan or a diff.
    pub fn text(&self) -> String {
        self.files
            .iter()
            .map(|f| f.contents.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The operation count, which the manifest and the release scan both read.
    pub fn operation_count(&self) -> usize {
        self.files
            .iter()
            .filter(|f| f.path.ends_with("client.ts") || f.path.ends_with("client.py"))
            .map(|_| 1)
            .sum::<usize>()
            .max(1)
    }
}

/// Generate a package for `language` from `document`.
///
/// `pinned_hash` is the caller's, not this function's. If the document's own hash differs, the
/// call is refused: a release that says it was built from hash A while the bytes on disk are
/// hash B is worse than no release, because the pin is the only thing that makes a generated
/// client reviewable.
pub fn generate(document: &Value, language: Language, pinned_hash: &str) -> Result<Package> {
    let canonical = canonical_json(document);
    let actual = Drift::openapi_hash(&canonical);
    if pinned_hash != actual {
        return Err(defect(format!(
                "the document hashes to {actual}, not the pinned {pinned_hash}; \
                 a client published from a different document than it claims is not reviewable"
            ),
        ));
    }
    let ops = operations(document)?;
    let api_version = document
        .get("info")
        .and_then(|i| i.get("version"))
        .and_then(Value::as_str)
        .unwrap_or("v1")
        .to_string();

    let files = match language {
        Language::TypeScript => typescript_files(&ops, pinned_hash, &api_version),
        Language::Python => python_files(&ops, pinned_hash, &api_version),
    };
    let package = Package {
        language,
        files,
        openapi_hash: pinned_hash.to_string(),
        api_version,
    };
    // The scan runs on our own output on every generation, not in a release job somebody can
    // forget. A generator that can emit a hostname is a generator that eventually does.
    let findings = scan_for_secrets(&package.text());
    if !findings.is_empty() {
        return Err(defect(format!(
                "the generated {} package contains {} value(s) that must never ship: {}",
                language.as_str(),
                findings.len(),
                findings.join(", ")
            ),
        ));
    }
    Ok(package)
}

// ---------------------------------------------------------------------------------------------
// TypeScript
// ---------------------------------------------------------------------------------------------

/// Escape a string for a TypeScript single-quoted literal.
///
/// Summaries come from annotations a human wrote, and a summary containing an apostrophe
/// (`GET a caller's key`) is not an exotic input — it is what this repository's own summaries
/// look like. Without the escape the generated file does not parse, and the failure is a line
/// number pointing into generated code nobody wrote.
fn ts_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(ch),
        }
    }
    out.push('\'');
    out
}

/// `get_developer_api-keys_by_id` → `getDeveloperApiKeysById`.
///
/// The document's ids are snake_case; a generated client should read as the language it is
/// generated into, and `getDeveloperApiKeysById` is what a TypeScript caller expects to type.
fn ts_camel(id: &str) -> String {
    let id = sanitize_identifier(id);
    let mut out = String::with_capacity(id.len());
    let mut upper = false;
    for ch in id.chars() {
        if ch == '_' {
            upper = true;
            continue;
        }
        if upper {
            out.extend(ch.to_uppercase());
            upper = false;
        } else {
            out.push(ch);
        }
    }
    out
}

/// `get` → `Get`, `delete` → `Delete`. A method named `get` on a class is legal but a property
/// read is not a call; the grouped form avoids the ambiguity entirely.
fn ts_group(tag: &str) -> String {
    ts_pascal(&sanitize_identifier(tag))
}

/// PascalCase a sanitised identifier, so a method name is never a reserved word or a digit.
///
/// **The sanitiser runs first, and the gate exists because it once did not.** This function
/// capitalised letters and dropped hyphens, which handled `/backup-schedules` and `/auth/step-up`
/// — and passed a tag containing a dot straight through, so the emitted member was
/// `Openapi.json()` and the file did not parse. Same shape as the hyphens, found the same way.
fn ts_pascal(identifier: &str) -> String {
    let mut out = String::with_capacity(identifier.len());
    let mut upper = true;
    for ch in identifier.chars() {
        if ch == '_' {
            upper = true;
            continue;
        }
        if upper {
            out.extend(ch.to_uppercase());
            upper = false;
        } else {
            out.push(ch);
        }
    }
    if out.is_empty() {
        // A tag that is entirely punctuation would otherwise produce a method named ``, which
        // parses as nothing at all. `Group` is a name no tag can collide with, because a tag
        // that sanitises to empty is a document defect worth seeing rather than papering over.
        return "Group".to_string();
    }
    out
}

fn typescript_files(
    ops: &[Operation],
    pinned_hash: &str,
    api_version: &str,
) -> Vec<GeneratedFile> {
    let mut client = String::new();
    let _ = writeln!(client, "{HEADER_TS}");
    let _ = writeln!(client, "export const OPENAPI_HASH = {};", ts_string(pinned_hash));
    let _ = writeln!(client, "export const API_VERSION = {};", ts_string(api_version));
    client.push('\n');
    // Declared here and emitted into `index.ts`, which is the entry point: the class body is
    // generated in one function and the transport in another, so the dependency is explicit in
    // both directions rather than implied by file order.
    let _ = writeln!(client, "import {{ OmnionRoutes }} from './index';");
    // client.ts must also re-export the spec type, which `./group` imports as a type — a value
    // import of a type-only module is erased at build time and leaves the annotation unresolved.
    let _ = writeln!(client, "export type {{ OperationSpec }} from './index';");
    client.push('\n');
    client.push_str(
        "export interface RequestOptions {\n  method: string;\n  path: string;\n  body?: unknown;\n}\n\n",
    );
    client.push_str(
        "/** One operation, exactly as the document describes it. */\n\
         export interface OperationSpec {\n  readonly id: string;\n  readonly method: string;\n  \
         readonly path: string;\n  readonly permission: string;\n  readonly deprecated: boolean;\n}\n\n",
    );
    client.push_str("export const OPERATIONS: readonly OperationSpec[] = [\n");
    for op in ops {
        let _ = writeln!(
            client,
            "  {{ id: {id}, method: {method}, path: {path}, permission: {perm}, deprecated: {dep} }},",
            id = ts_string(&op.id),
            method = ts_string(&op.method),
            path = ts_string(&op.path),
            perm = ts_string(&op.permission),
            dep = op.deprecated,
        );
    }
    client.push_str("];\n\n");
    client.push_str(TS_CLIENT_BODY);

    let mut by_group = BTreeMap::new();
    for op in ops {
        by_group.entry(ts_group(&op.tag)).or_insert_with(Vec::new).push(op);
    }
    // **The groups live inside the client class.** The first version emitted them at file top
    // level — a bare `Ai(): OperationGroup {` with no enclosing `class` — and the file did not
    // parse. The gate below is the reason it was caught before a release rather than after: three
    // separate defects, all of them invisible to a unit test over the generator's own functions,
    // and every one of them a package that could never be imported.
    let mut groups = String::new();
    let _ = writeln!(groups, "/** Every route this document describes, grouped by tag. */");
    let _ = writeln!(groups, "export class OmnionRoutes {{");
    for (group, members) in &by_group {
        let _ = writeln!(groups, "  /** {group} */");
        // A plain method, not `get x()`. One tag in this platform is `ai`, and `get ai()` is
        // the JavaScript accessor for a property named `ai` — the generated file does not parse.
        // A getter also invites assignment, and there is nothing here to assign to.
        let _ = writeln!(groups, "  {group}(): OperationGroup {{");
        let _ = writeln!(
            groups,
            "    return new OperationGroup({{ name: {name}, operations: [",
            name = ts_string(group)
        );
        for op in members {
            let _ = writeln!(groups, "      OPERATIONS_BY_ID[{id}]!,", id = ts_string(&op.id));
        }
        groups.push_str("    ] });\n  }\n\n");
    }
    groups.push_str("  /** Every group, by name. */\n  all(): Record<string, OperationGroup> {\n    return {\n");
    for (group, _) in &by_group {
        let _ = writeln!(
            groups,
            "      {name}: this.{group}(),",
            name = ts_string(group)
        );
    }
    groups.push_str("    };\n  }\n}\n");

    let mut index = String::new();
    let _ = writeln!(index, "{HEADER_TS}");
    let _ = writeln!(index, "// Generated by the Omnion SDK generator. Do not edit by hand.");
    let _ = writeln!(index, "export * from './client';");
    // The group class has to be imported by BUNDLE, not just re-exported by name: the first
    // version wrote `export { OperationGroup } from './client'` and the file parsed, imported
    // cleanly, and threw a ReferenceError the first time a group method ran.
    let _ = writeln!(index, "import {{ OperationGroup }} from './group';");
    // `index.ts` BUILDS the route table, so it needs the two values it indexes into. The first
    // version imported only the class and the package threw `ReferenceError: Can't find
    // variable: OPERATIONS_BY_ID` on the first group call — a second run-time failure of exactly
    // the same shape as the one above, which is why every cross-module name is now imported
    // explicitly instead of assumed to be in scope.
    let _ = writeln!(index, "import {{ OPERATIONS, OPERATIONS_BY_ID, OperationSpec }} from './client';");
    index.push('\n');
    let _ = writeln!(
        index,
        "export const OPENAPI_HASH = {};\n",
        ts_string(pinned_hash)
    );
    let _ = writeln!(index, "export const API_VERSION = {};", ts_string(api_version));
    index.push('\n');
    index.push_str(&groups);

    let mut package_json = String::new();
    let _ = writeln!(package_json, "{{");
    let _ = writeln!(package_json, "  \"name\": \"@omnion/api-client\",");
    let _ = writeln!(package_json, "  \"version\": \"0.1.0\",");
    let _ = writeln!(package_json, "  \"description\": \"Generated from the Omnion OpenAPI document.\",");
    let _ = writeln!(package_json, "  \"type\": \"module\",");
    let _ = writeln!(package_json, "  \"main\": \"dist/index.js\",");
    let _ = writeln!(package_json, "  \"types\": \"dist/index.d.ts\",");
    let _ = writeln!(package_json, "  \"license\": \"Apache-2.0\",");
    let _ = writeln!(package_json, "  \"omnion\": {{");
    let _ = writeln!(package_json, "    \"openapiHash\": {},", ts_string(pinned_hash));
    let _ = writeln!(package_json, "    \"apiVersion\": {}", ts_string(api_version));
    let _ = writeln!(package_json, "  }}");
    let _ = writeln!(package_json, "}}");

    let readme = format!(
        "# @omnion/api-client\n\n\
         Generated from the Omnion OpenAPI document `{hash}` (API version `{version}`).\n\n\
         **Do not edit by hand.** Re-run the generator; edits are lost on the next release and\n\
         the pinned hash would no longer describe the bytes in this package.\n\n\
         Every call is a method on a group named after the route's tag:\n\n\
         ```ts\n\
         const client = new OmnionClient({{ baseUrl, token }});\n\
         const pages = await client.Developer.getDeveloperApiKeysById('key_123');\n\
         ```\n\n\
         A call answers `OmnionError` for a non-2xx response, with the status and the body's\n\
         `message` when the platform sent one.\n",
        hash = pinned_hash,
        version = api_version,
    );

    vec![
        GeneratedFile { path: "package.json".into(), contents: package_json },
        GeneratedFile { path: "README.md".into(), contents: readme },
        GeneratedFile { path: "src/index.ts".into(), contents: index },
        GeneratedFile { path: "src/client.ts".into(), contents: client },
        GeneratedFile { path: "src/group.ts".into(), contents: TS_GROUP_MODULE.to_string() },
    ]
}

/// The transport half, kept in its own string so the generated table above and the code below
/// read as one file when a human opens the published package.
const TS_CLIENT_BODY: &str = r#"/**
 * The id table. Exported because `index.ts` builds the per-tag route lists out of it, and a
 * value used across two generated modules has to be importable — a `const` with no `export` is
 * invisible to the other file and the failure is a runtime `ReferenceError`, not a compile error.
 */
export const OPERATIONS_BY_ID: Readonly<Record<string, OperationSpec>> =
  Object.fromEntries(OPERATIONS.map((operation) => [operation.id, operation]));

/** A non-2xx answer, carrying whatever the platform's error envelope said. */
export class OmnionError extends Error {
  constructor(
    readonly status: number,
    readonly operationId: string,
    message: string,
  ) {
    super(message);
    this.name = 'OmnionError';
  }
}

/** The routes under one tag. */

export interface OmnionClientOptions {
  baseUrl: string;
  token?: string;
  fetch?: typeof globalThis.fetch;
}

/** The generated client. */
export class OmnionClient {
  private readonly baseUrl: string;
  private readonly token: string | undefined;
  private readonly doFetch: typeof globalThis.fetch;

  constructor(options: OmnionClientOptions) {
    this.baseUrl = options.baseUrl.replace(/\/$/, '');
    this.token = options.token;
    this.doFetch = options.fetch ?? globalThis.fetch;
  }

  /** Every route in the document, grouped by tag. */
  readonly routes = new OmnionRoutes();

  /** Call any operation by id. The groups below are typed sugar over this. */
  async call<T = unknown>(id: string, args: Record<string, string> = {}): Promise<T> {
    const operation = OPERATIONS_BY_ID[id];
    if (!operation) {
      throw new OmnionError(404, id, `No operation named \`${id}\` exists in this document.`);
    }
    const path = operation.path.replace(/\{([^}]+)\}/g, (_match, name: string) => {
      const value = args[name];
      if (value === undefined) {
        throw new OmnionError(0, id, `\`${id}\` needs a value for \`${name}\`.`);
      }
      return encodeURIComponent(value);
    });
    const response = await this.doFetch(`${this.baseUrl}${path}`, {
      method: operation.method,
      headers: {
        accept: 'application/json',
        ...(this.token ? { authorization: `Bearer ${this.token}` } : {}),
      },
    });
    if (!response.ok) {
      const body = (await response.json().catch(() => null)) as { message?: string } | null;
      throw new OmnionError(response.status, id, body?.message ?? response.statusText);
    }
    if (response.status === 204) {
      return undefined as T;
    }
    return (await response.json()) as T;
  }
}
"#;

/// Appended to `index.ts` after the generated route groups.
///
/// Declared in `client.ts` and used in `index.ts` because the two files are generated by one
/// function and the ordering of a `class` body's members is not a thing a format string can
/// carry; the alternative — generating the class twice into both files — is a second source of
/// truth for the same member list, which is the drift this whole slice exists to prevent.
/// `OperationGroup` on its own, because `client.ts` and `index.ts` both need it.
///
/// The first version kept it in `client.ts` and had `index.ts` re-export it from there, which
/// is a cycle: `client.ts` imports `OmnionRoutes` from `index.ts` for the `routes` property, and
/// `index.ts` would import `OperationGroup` back from `client.ts`. Bun resolved it far enough to
/// parse and then threw `ReferenceError: Can't find variable: OperationGroup` at the first call —
/// **a defect no compile check can see**, because a cycle between two modules that both import
/// successfully still fails at run time. A third file is the whole fix.
const TS_GROUP_MODULE: &str = r#"import type { OperationSpec } from './client';

export class OperationGroup {
  constructor(readonly info: { name: string; operations: readonly OperationSpec[] }) {}

  /** Ids of every operation in this group — what the playground's copy-as-code menu reads. */
  ids(): readonly string[] {
    return this.info.operations.map((operation) => operation.id);
  }
}
"#;

const HEADER_TS: &str = "// Generated by the Omnion SDK generator from the committed OpenAPI document.\n\
                        // Do not edit by hand: the pinned hash describes these bytes, and an edit\n\
                        // makes the package claim a provenance it no longer has.\n";

// ---------------------------------------------------------------------------------------------
// Python
// ---------------------------------------------------------------------------------------------

/// Escape a string for a Python single-quoted literal.
///
/// Python has no escape for a single quote inside a single-quoted string, so a backslash is
/// inserted — which is why this cannot be shared with the TypeScript path by copy-paste.
fn py_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(ch),
        }
    }
    out.push('\'');
    out
}

/// `get_developer_api-keys_by_id` → `get_developer_api_keys_by_id`, plus a Python keyword guard.
fn py_function(id: &str) -> String {
    const KEYWORDS: [&str; 35] = [
        "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class",
        "continue", "def", "del", "elif", "else", "except", "finally", "for", "from", "global",
        "if", "import", "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return",
        "try", "while", "yield", "match",
    ];
    let name = id.replace(['-', '.', ' '], "_");
    if KEYWORDS.contains(&name.as_str()) {
        format!("{name}_")
    } else {
        name
    }
}

fn python_files(ops: &[Operation], pinned_hash: &str, api_version: &str) -> Vec<GeneratedFile> {
    let mut client = String::new();
    let _ = writeln!(client, "{HEADER_PY}");
    let _ = writeln!(client, "OPENAPI_HASH = {hash}", hash = py_string(pinned_hash));
    let _ = writeln!(client, "API_VERSION = {version}", version = py_string(api_version));
    // Emitted row by row rather than from one `format!` with a repetition: a row is a dict
    // literal, so its braces have to be escaped for the formatter, and 496 rows of escaped braces
    // is a wall of `{{{{` that hides the values. The first version also opened the list with a
    // placeholder row and then truncated it back with `rfind(']')` — which found the `]` inside
    // the PLACEHOLDER and left the list unterminated, so the file did not parse.
    client.push_str("\nOPERATIONS = [\n");
    for op in ops {
        let _ = writeln!(
            client,
            "    {{'id': {id}, 'method': {method}, 'path': {path}, 'permission': {perm}, 'deprecated': {dep}}},",
            id = py_string(&op.id),
            method = py_string(&op.method),
            path = py_string(&op.path),
            perm = py_string(&op.permission),
            dep = if op.deprecated { "True" } else { "False" },
        );
    }
    client.push_str("]\n");
    client.push_str("\nOPERATIONS_BY_ID = {entry['id']: entry for entry in OPERATIONS}\n");
    client.push_str(PY_CLIENT_BODY);

    let mut by_group = BTreeMap::new();
    for op in ops {
        by_group.entry(op.tag.clone()).or_insert_with(Vec::new).push(op);
    }
    let mut groups = String::new();
    for (group, members) in &by_group {
        let _ = writeln!(groups, "class {name}:", name = py_class_name(group));
        let _ = writeln!(groups, "    \"\"\"Routes under `{tag}`.\"\"\"", tag = group);
        groups.push_str("\n    @staticmethod\n    def ids() -> list:\n        return [\n");
        for op in members {
            let _ = writeln!(groups, "            {id},", id = py_string(&op.id));
        }
        groups.push_str("        ]\n\n");
        for op in members {
            let (template, consumed) = op
                .path_template()
                .expect("the document is validated when it is read");
            let args = consumed
                .iter()
                .map(|n| format!("{n}: str"))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(groups, "    @staticmethod");
            let _ = writeln!(groups, "    def {name}({args}) -> dict:", name = py_function(&op.id));
            let _ = writeln!(groups, "        \"\"\"`{path}`.\"\"\"", path = op.path);
            if consumed.is_empty() {
                let _ = writeln!(groups, "        return call({id})", id = py_string(&op.id));
            } else {
                // The DICT passes the values, never their annotations. The first version built
                // one string for both and produced `call(id, {scope: str, slot: str, ...})` —
                // which is a `SyntaxError` in every file with a path parameter, and the generator
                // had 496 operations' worth of green unit tests over it.
                let kwargs = consumed
                    .iter()
                    .map(|n| format!("'{n}': {n}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = writeln!(
                    groups,
                    "        return call({id}, {{{kwargs}}})",
                    id = py_string(&op.id),
                    kwargs = kwargs
                );
            }
            let _ = args;
            let _ = template;
        }
    }

    let mut init = String::new();
    let _ = writeln!(init, "{HEADER_PY}");
    init.push_str("from .client import (\n    OmnionClient,\n    OmnionError,\n    OPERATIONS,\n    OPERATIONS_BY_ID,\n    OPENAPI_HASH,\n    API_VERSION,\n    call,\n)\n");
    init.push_str("from . import groups\n\n__all__ = [\n    'OmnionClient',\n    'OmnionError',\n    'OPERATIONS',\n    'OPERATIONS_BY_ID',\n    'OPENAPI_HASH',\n    'API_VERSION',\n    'call',\n    'groups',\n]\n");

    let mut pyproject = String::new();
    let _ = writeln!(pyproject, "[build-system]");
    let _ = writeln!(pyproject, "requires = [\"hatchling\"]");
    let _ = writeln!(pyproject, "build-backend = \"hatchling.build\"");
    let _ = writeln!(pyproject);
    let _ = writeln!(pyproject, "[project]");
    let _ = writeln!(pyproject, "name = \"omnion-api-client\"");
    let _ = writeln!(pyproject, "version = \"0.1.0\"");
    let _ = writeln!(pyproject, "description = \"Generated from the Omnion OpenAPI document.\"");
    let _ = writeln!(pyproject, "license = {{ text = \"Apache-2.0\" }}");
    let _ = writeln!(pyproject);
    let _ = writeln!(pyproject, "[project.optional-dependencies]");
    let _ = writeln!(pyproject, "dev = [\"httpx>=0.27\"]");
    let _ = writeln!(pyproject);
    let _ = writeln!(pyproject, "[tool.hatch.build.targets.wheel]");
    let _ = writeln!(pyproject, "packages = [\"src/omnion_api_client\"]");

    let readme = format!(
        "# omnion-api-client\n\n\
         Generated from the Omnion OpenAPI document `{hash}` (API version `{version}`).\n\n\
         **Do not edit by hand.** Re-run the generator; the pinned hash describes these bytes.\n\n\
         ```python\n\
         from omnion_api_client import OmnionClient\n\n\
         client = OmnionClient(base_url, token=token)\n\
         keys = client.call('get_developer_api_keys')\n\
         ```\n",
        hash = pinned_hash,
        version = api_version,
    );

    vec![
        GeneratedFile { path: "pyproject.toml".into(), contents: pyproject },
        GeneratedFile { path: "README.md".into(), contents: readme },
        GeneratedFile { path: "src/omnion_api_client/__init__.py".into(), contents: init },
        GeneratedFile { path: "src/omnion_api_client/client.py".into(), contents: client },
        GeneratedFile { path: "src/omnion_api_client/groups.py".into(), contents: groups },
    ]
}

fn py_class_name(tag: &str) -> String {
    let mut out = String::with_capacity(tag.len());
    let mut upper = true;
    for ch in sanitize_identifier(tag).chars() {
        if ch == '_' {
            upper = true;
            continue;
        }
        if upper {
            out.extend(ch.to_uppercase());
            upper = false;
        } else {
            out.push(ch);
        }
    }
    out
}

const HEADER_PY: &str = "\"\"\"Generated by the Omnion SDK generator from the committed OpenAPI document.\n\nDo not edit by hand: the pinned hash describes these bytes.\n\"\"\"\n";

const PY_CLIENT_BODY: &str = r#"
import json
import urllib.error
import urllib.parse
import urllib.request


class OmnionError(Exception):
    """A non-2xx answer, carrying whatever the platform's error envelope said."""

    def __init__(self, status: int, operation_id: str, message: str) -> None:
        super().__init__(message)
        self.status = status
        self.operation_id = operation_id


def _resolve(operation: dict, args: dict) -> str:
    path = operation['path']
    for name, value in args.items():
        path = path.replace('{' + name + '}', urllib.parse.quote(str(value), safe=''))
    if '{' in path:
        raise OmnionError(0, operation['id'], f"`{operation['id']}` is missing a path argument.")
    return path


def call(base_url: str, token: str | None, operation_id: str, args: dict | None = None) -> dict:
    """Call any operation by id."""
    operation = OPERATIONS_BY_ID.get(operation_id)
    if operation is None:
        raise OmnionError(404, operation_id, f'No operation named `{operation_id}` exists in this document.')
    request = urllib.request.Request(
        base_url.rstrip('/') + _resolve(operation, args or {}),
        method=operation['method'],
        headers={
            'accept': 'application/json',
            **({'authorization': f'Bearer {token}'} if token else {}),
        },
    )
    try:
        with urllib.request.urlopen(request) as response:  # noqa: S310 - the caller supplies the host
            body = response.read()
    except urllib.error.HTTPError as error:
        raw = error.read()
        try:
            message = json.loads(raw).get('message') or error.reason
        except (ValueError, AttributeError):
            message = error.reason
        raise OmnionError(error.code, operation_id, str(message)) from error
    if not body:
        return {}
    return json.loads(body)


class OmnionClient:
    """The generated client."""

    def __init__(self, base_url: str, token: str | None = None) -> None:
        self.base_url = base_url
        self.token = token

    def call(self, operation_id: str, args: dict | None = None) -> dict:
        return call(self.base_url, self.token, operation_id, args)
"#;

// ---------------------------------------------------------------------------------------------
// The secret scan
// ---------------------------------------------------------------------------------------------

/// Report anything in a generated package that must never ship.
///
/// This is a **shape** check, not a grep for today's leaks, and the difference matters. A grep
/// finds `https://acme.example` only after someone has committed it. A shape check asks the
/// question that generates the leak: does this package contain a scheme, a credential, or a
/// token-shaped string at all? A client is generated from paths, verbs and summaries, so the
/// answer should always be *no*, and a `yes` names the rule that was broken rather than the
/// string someone happened to paste.
///
/// The four shapes:
/// - `scheme://` — any absolute URL, so no hostnames and no environment endpoints,
/// - `user:pass@` — credentials embedded in a DSN,
/// - `Bearer <literal>` — a token in the source rather than in a header the caller sets,
/// - `password`/`secret`/`api_key` assigned a quoted literal.
pub fn scan_for_secrets(text: &str) -> Vec<String> {
    let mut findings = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        // A comment naming a scheme is documentation, not a value; every rule below is about
        // code, and a generator's own header is allowed to say "this file has no host in it".
        let is_comment = trimmed.starts_with("//")
            || trimmed.starts_with("#")
            || trimmed.starts_with("*")
            || trimmed.starts_with("///")
            || trimmed.starts_with("\"\"\"");
        if is_comment {
            continue;
        }
        if has_absolute_url(line) {
            findings.push(format!("line {}: an absolute URL", number + 1));
        }
        if has_embedded_credentials(line) {
            findings.push(format!("line {}: credentials in a connection string", number + 1));
        }
        if has_literal_bearer(line) {
            findings.push(format!("line {}: a literal bearer token", number + 1));
        }
        if has_assigned_secret(line) {
            findings.push(format!("line {}: a secret assigned a literal", number + 1));
        }
    }
    findings
}

fn has_absolute_url(line: &str) -> bool {
    for scheme in ["http://", "https://", "postgres://", "postgresql://", "redis://", "amqp://"] {
        if line.contains(scheme) {
            return true;
        }
    }
    false
}

fn has_embedded_credentials(line: &str) -> bool {
    // `://` then something then `@`, with no `/` after the scheme — a DSN, not a path.
    let Some(start) = line.find("://") else { return false };
    let rest = &line[start + 3..];
    let authority: &str = rest.split(['/', '?', '#']).next().unwrap_or("");
    authority.contains('@') && authority.contains(':')
}

fn has_literal_bearer(line: &str) -> bool {
    let Some(at) = line.find("Bearer ") else { return false };
    // The generated clients write `Bearer ${token}` and `f'Bearer {token}'` — a template, not a
    // value. A literal is anything after `Bearer ` that is not an interpolation.
    let value = line[at + 7..].trim();
    !value.starts_with('$')
        && !value.starts_with('{')
        && !value.starts_with('`')
        && !value.is_empty()
}

fn has_assigned_secret(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    if !(lower.contains("password")
        || lower.contains("secret")
        || lower.contains("api_key")
        || lower.contains("apikey")
        || lower.contains("token"))
    {
        return false;
    }
    let Some(eq) = line.find(['=', ':']) else { return false };
    let value = line[eq + 1..].trim();
    // A quoted literal, in either quoting style. Written as byte comparisons rather than
    // `starts_with('"')` because the two-character literals `'"'` and `'b"'` put a quote inside a
    // quote inside a quote, and that is how the first version of this function made the whole
    // file unparseable with an error pointing 400 lines away from the line that broke it.
    const DOUBLE: u8 = b'"';
    const SINGLE: u8 = b'\'';
    // The value is trimmed of trailing statement punctuation first. `const api_key = "abc";`
    // ends in `;`, so without this the check asks whether a line ending in a semicolon ends in
    // a quote — it does not, and the one shape this rule exists for passes through. Found by the
    // test below failing on the exact case it was written for.
    let value = value.trim_end_matches([';', ',']).trim();
    let bytes = value.as_bytes();
    let quoted = |open: u8, close: u8| {
        bytes.len() > 1 && bytes[0] == open && bytes[bytes.len() - 1] == close
    };
    // A declaration with no value, a comparison, or a call is not an assignment of a literal.
    quoted(DOUBLE, DOUBLE) || quoted(SINGLE, SINGLE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openapi::RouteEntry;
    use serde_json::json;

    /// A summary with an apostrophe in it, which is the shape this repository's own
    /// annotations already produce (`GET a caller's key`). Held in a constant so the escaping
    /// assertions below read as data rather than as a line of nested quoting.
    /// The apostrophe itself, held as a `char` so the summary constants below never have to nest
    /// a quote inside a quote inside a quote — a nesting that is the single largest source of
    /// parse errors in generated-source tests, and the reason this file has one constant for it
    /// rather than a literal in each assertion.
    const APOSTROPHE: char = '\'';
    const SUMMARY_WITH_APOSTROPHE: &str = "GET a caller";
    const SUMMARY_TAIL: &str = "s key";

    fn sample_document() -> Value {
        json!({
            "openapi": "3.1.0",
            "info": { "title": "Omnion API", "version": "v1" },
            "paths": {
                "/backup-schedules": {
                    "get": {
                        "operationId": "get_backup_schedules",
                        "summary": "GET schedules",
                        "tags": ["backup-schedules"],
                        "x-omnion-permission": "deployment.read"
                    }
                },
                "/developer/api-keys/{id}": {
                    "delete": {
                        "operationId": "delete_developer_api_keys_by_id",
                        "summary": "DELETE a caller's key",
                        "tags": ["developer"],
                        "x-omnion-permission": "developer.keys.manage"
                    }
                },
                "/health": {
                    "get": { "operationId": "get_health", "summary": "GET health", "tags": ["health"] }
                }
            }
        })
    }

    fn pinned(document: &Value) -> String {
        Drift::openapi_hash(&canonical_json(document))
    }

    // --- reading the document ------------------------------------------------------------------

    #[test]
    fn every_operation_is_read_with_the_permission_the_guard_enforces() {
        let ops = operations(&sample_document()).expect("the sample document is well formed");
        assert_eq!(ops.len(), 3);
        let revoke = ops
            .iter()
            .find(|o| o.id == "delete_developer_api_keys_by_id")
            .expect("the delete operation is read");
        assert_eq!(revoke.permission, "developer.keys.manage");
        assert_eq!(revoke.parameters, vec!["id"]);
        // The unguarded route reads as unguarded rather than borrowing a neighbour's key.
        let health = ops.iter().find(|o| o.id == "get_health").expect("health is read");
        assert_eq!(health.permission, "");
    }

    #[test]
    fn operations_come_back_in_a_deterministic_order() {
        let first = operations(&sample_document()).expect("readable");
        let second = operations(&sample_document()).expect("readable");
        let ids = |ops: Vec<Operation>| ops.into_iter().map(|o| o.id).collect::<Vec<_>>();
        assert_eq!(ids(first), ids(second));
        // Sorted by TAG, so a client's methods group the way a human reads them: the
        // `backup-schedules` group precedes `developer` alphabetically regardless of the order
        // the operations appear in the document. Asserted explicitly because "deterministic" and
        // "grouped" are different claims and only the second one is what a caller wants.
        assert_eq!(
            ids(operations(&sample_document()).expect("readable")),
            vec!["get_backup_schedules", "delete_developer_api_keys_by_id", "get_health"]
        );
        let tags = operations(&sample_document())
            .expect("readable")
            .into_iter()
            .map(|o| o.tag)
            .collect::<Vec<_>>();
        assert_eq!(tags, vec!["backup-schedules", "developer", "health"]);
    }

    #[test]
    fn a_duplicate_operation_id_is_refused_rather_than_silently_overwritten() {
        let mut document = sample_document();
        document["paths"]["/developer/api-keys/{id}"]["post"] = json!({
            "operationId": "delete_developer_api_keys_by_id",
            "summary": "POST under a duplicate id",
            "tags": ["developer"]
        });
        let error = operations(&document).expect_err("a duplicate id is a document defect");
        assert!(
            error.to_string().contains("twice"),
            "the error should name the duplication: {}",
            error.to_string()
        );
    }

    #[test]
    fn an_operation_with_no_id_is_refused_rather_than_named_after_its_path() {
        let document = json!({
            "info": { "version": "v1" },
            "paths": { "/x": { "get": { "summary": "no id" } } }
        });
        let error = operations(&document).expect_err("a nameless operation is unusable");
        assert!(error.to_string().contains("operationId"), "{}", error.to_string());
    }

    // --- path templates -----------------------------------------------------------------------

    #[test]
    fn a_path_parameter_becomes_positional_and_is_reported() {
        let op = Operation {
            id: "get_x_by_id".into(),
            method: "GET".into(),
            path: "/developer/api-keys/{id}".into(),
            summary: String::new(),
            tag: "developer".into(),
            permission: String::new(),
            deprecated: false,
            parameters: vec!["id".into()],
        };
        let (template, consumed) = op.path_template().expect("the pattern uses the parameter");
        assert_eq!(template, "/developer/api-keys/{0}");
        assert_eq!(consumed, vec!["id"]);
    }

    #[test]
    fn a_parameter_the_pattern_never_uses_is_an_error_not_a_silent_drop() {
        let op = Operation {
            id: "get_x".into(),
            method: "GET".into(),
            path: "/x".into(),
            summary: String::new(),
            tag: "x".into(),
            permission: String::new(),
            deprecated: false,
            parameters: vec!["id".into()],
        };
        let error = op.path_template().expect_err("an unused parameter is a defect");
        assert!(error.to_string().contains("does not use it"), "{}", error.to_string());
    }

    // --- the pin ------------------------------------------------------------------------------

    #[test]
    fn a_pin_that_does_not_match_the_document_is_refused() {
        let document = sample_document();
        let error = generate(&document, Language::TypeScript, "sha256:0000")
            .expect_err("a client built from another document is not reviewable");
        assert!(error.to_string().contains("not the pinned"), "{}", error.to_string());
    }

    #[test]
    fn generation_is_reproducible_from_the_same_document() {
        let document = sample_document();
        let hash = pinned(&document);
        let first = generate(&document, Language::TypeScript, &hash).expect("generates");
        let second = generate(&document, Language::TypeScript, &hash).expect("generates");
        assert_eq!(first, second, "the same document must produce the same bytes");
        let py = generate(&document, Language::Python, &hash).expect("generates");
        assert_eq!(py, generate(&document, Language::Python, &hash).expect("generates"));
    }

    #[test]
    fn the_package_carries_the_hash_it_was_built_from() {
        let document = sample_document();
        let hash = pinned(&document);
        for language in Language::all() {
            let package = generate(&document, language, &hash).expect("generates");
            assert_eq!(package.openapi_hash, hash);
            assert!(package.text().contains(&hash), "the hash must appear in the package");
            assert!(
                package.text().contains("API_VERSION") || package.text().contains("API_VERSION"),
                "the API version is part of the provenance"
            );
        }
    }

    // --- output shape -------------------------------------------------------------------------

    #[test]
    fn every_operation_appears_in_both_languages() {
        let document = sample_document();
        let hash = pinned(&document);
        let ts = generate(&document, Language::TypeScript, &hash).expect("generates");
        let py = generate(&document, Language::Python, &hash).expect("generates");
        for op in operations(&document).expect("readable") {
            assert!(ts.text().contains(&op.id), "TypeScript is missing {}", op.id);
            assert!(py.text().contains(&op.id), "Python is missing {}", op.id);
            assert!(ts.text().contains(&op.path), "TypeScript is missing the path {}", op.path);
            assert!(py.text().contains(&op.path), "Python is missing the path {}", op.path);
        }
    }

    #[test]
    fn a_summary_containing_an_apostrophe_survives_both_escapers() {
        // The real repository writes `GET a caller's key`-shaped summaries, and an unescaped
        // apostrophe produces a package that does not parse — with the error pointing at a line
        // in generated code.
        let summary = [SUMMARY_WITH_APOSTROPHE, &APOSTROPHE.to_string(), SUMMARY_TAIL].concat();
        assert_eq!(ts_string(&summary), "'GET a caller\\'s key'");
        assert_eq!(py_string(&summary), "'GET a caller\\'s key'");
        assert_eq!(ts_string("a \\ backslash"), "'a \\\\ backslash'");
        assert_eq!(py_string("a \\ backslash"), "'a \\\\ backslash'");
    }

    #[test]
    fn the_typescript_client_uses_camel_case_methods_over_snake_case_ids() {
        let document = sample_document();
        let hash = pinned(&document);
        let ts = generate(&document, Language::TypeScript, &hash).expect("generates");
        assert!(ts.text().contains("getDeveloperApiKeysById"), "ids should be camel-cased in TS");
        // The raw id stays in the table — it is the wire name the playground copies.
        assert!(ts.text().contains("delete_developer_api_keys_by_id"));
        // A hyphenated tag becomes a legal class/property name.
        assert!(ts.text().contains("BackupSchedules"), "hyphenated tags must be sanitised");
    }

    #[test]
    fn python_function_names_escape_keywords() {
        // The escaper is called on a bare name here, not on an operation id, because an id always
        // begins with the HTTP verb and can therefore never BE a keyword. Testing it with
        // `get_import` (which is not a keyword, and whose underscore suffix the first version of
        // this test wrongly expected) proved nothing about either case; these two do.
        assert_eq!(py_function("import"), "import_");
        assert_eq!(py_function("class"), "class_");
        assert_eq!(py_function("lambda"), "lambda_");
        // A real id is untouched, because it is already a legal Python function name.
        assert_eq!(py_function("get_backup-schedules"), "get_backup_schedules");
        assert_eq!(py_function("delete_developer_api_keys_by_id"), "delete_developer_api_keys_by_id");
    }

    // --- the secret scan ----------------------------------------------------------------------

    #[test]
    fn a_generated_package_carries_nothing_that_must_not_ship() {
        let document = sample_document();
        let hash = pinned(&document);
        for language in Language::all() {
            let package = generate(&document, language, &hash).expect("generates");
            assert!(
                scan_for_secrets(&package.text()).is_empty(),
                "{}: {:?}",
                language.as_str(),
                scan_for_secrets(&package.text())
            );
        }
    }

    #[test]
    fn the_scan_catches_the_four_shapes_it_exists_for() {
        let cases = [
            ("const url = 'https://acme.example';", "absolute URL"),
            ("dsn = 'postgres://user:hunter2@db:5432/omnion'", "connection string"),
            ("const headers = { authorization: 'Bearer sk-live-abc' };", "bearer token"),
            ("const api_key = \"abc123\";", "secret literal"),
        ];
        for (line, what) in cases {
            let findings = scan_for_secrets(line);
            assert!(!findings.is_empty(), "{what} should be flagged: {line}");
        }
    }

    #[test]
    fn the_scan_does_not_flag_the_clients_own_transport() {
        // Both clients interpolate the caller's token. That is the whole design: the token is
        // the caller's, supplied at construction, and a scan that flagged it would force the
        // generator to invent a worse shape.
        for line in [
            "...(this.token ? { authorization: `Bearer ${this.token}` } : {}),",
            "**({'authorization': f'Bearer {token}'} if token else {}),",
            "this.token = options.token;",
            "    def __init__(self, base_url: str, token: str | None = None) -> None:",
        ] {
            assert!(
                scan_for_secrets(line).is_empty(),
                "false positive on {:?}: {:?}",
                line,
                scan_for_secrets(line)
            );
        }
    }

    #[test]
    fn a_comment_naming_a_scheme_is_documentation_not_a_leak() {
        assert!(scan_for_secrets("// the base URL is a https:// URL the caller supplies").is_empty());
        assert!(scan_for_secrets("# set OMNION_URL=https://your-host here").is_empty());
    }

    /// The acceptance line, and the only one in this slice that cannot be satisfied by the
    /// generator agreeing with itself.
    ///
    /// It says the packages **compile**. A unit test over `ts_string` proves the escaper returns
    /// what the author thinks it returns; it cannot notice that the emitted file does not parse,
    /// and the first version of this generator emitted 496 operations' worth of TypeScript with
    /// a method named `get ai()` and Python with `{scope: str}` where a value belonged. Both
    /// packages were wrong and every test was green.
    ///
    /// So the emitted text is written to a temporary directory and handed to the two toolchains
    /// that have to accept it. The test is skipped — loudly, on stderr — when a toolchain is
    /// absent, because a machine without `bun` is not a platform that should fail a release
    /// gate; a machine WITH it must not be able to publish an unparseable client.
    #[test]
    fn the_generated_packages_parse_in_their_own_toolchains() {
        let document = read_committed_snapshot();
        let hash = pinned(&document);
        let root = std::env::temp_dir().join(format!("omnion-sdk-parse-{}", std::process::id()));

        for language in Language::all() {
            let package = generate(&document, language, &hash)
                .unwrap_or_else(|e| panic!("{} generated nothing: {e}", language.as_str()));
            let dir = root.join(language.as_str());
            let _ = std::fs::remove_dir_all(&dir);
            for file in &package.files {
                let path = dir.join(&file.path);
                std::fs::create_dir_all(path.parent().expect("a file has a parent"))
                    .expect("the temp dir is writable");
                std::fs::write(&path, &file.contents).expect("the temp dir is writable");
            }

            let (program, args): (&str, Vec<String>) = match language {
                Language::TypeScript => (
                    "bun",
                    vec![
                        "build".into(),
                        dir.join("src/index.ts").display().to_string(),
                        "--target".into(),
                        "node".into(),
                        "--outdir".into(),
                        dir.join("out").display().to_string(),
                    ],
                ),
                Language::Python => (
                    "python3",
                    vec!["-m".into(), "compileall".into(), "-q".into(), dir.display().to_string()],
                ),
            };
            if !tool_exists(program) {
                eprintln!(
                    "skipping the {} parse check: `{program}` is not on PATH",
                    language.as_str()
                );
                continue;
            }
            let output = std::process::Command::new(program)
                .args(&args)
                .output()
                .unwrap_or_else(|e| panic!("could not run {program}: {e}"));
            // Both streams, because they fail differently: `python3 -m compileall` writes its
            // SyntaxError to STDOUT and leaves stderr empty, so a message built from stderr alone
            // reported "does not parse:" with nothing after it.
            assert!(
                output.status.success(),
                "the generated {} package does not parse:\n--- stdout ---\n{}\n--- stderr ---\n{}",
                language.as_str(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The committed snapshot, read the way a caller would read it.
    fn read_committed_snapshot() -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../api/openapi.snapshot.json");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    /// Whether a toolchain is on `PATH`.
    ///
    /// `which` is not used: it is a shell builtin wrapper with different behaviour per platform,
    /// and this only needs to know whether spawning the program would work.
    fn tool_exists(program: &str) -> bool {
        let Ok(path) = std::env::var("PATH") else { return false };
        std::env::split_paths(&path).any(|dir| {
            let candidate = dir.join(program);
            candidate.is_file()
        })
    }

    // --- the operation id fix this slice carried ------------------------------------------------

    #[test]
    fn operation_ids_are_legal_identifiers_in_both_languages() {
        for (method, path) in [
            ("GET", "/backup-schedules"),
            ("POST", "/auth/step-up"),
            ("DELETE", "/backups/{id}/restore-jobs"),
            ("GET", "/command-center/recent"),
        ] {
            let id = RouteEntry::new(method, path).operation_id();
            assert!(
                id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "`{id}` (from {method} {path}) is not a legal identifier"
            );
            assert!(!id.starts_with(|c: char| c.is_ascii_digit()), "`{id}` starts with a digit");
        }
    }

    #[test]
    fn sanitising_can_merge_two_routes_and_the_document_refuses_to_describe_both() {
        // `/step-up` and `/step_up` are two different endpoints, and sanitising — which is what
        // makes the ids legal in either language — maps them onto ONE id. The first version of
        // this test asserted they stayed apart, which is not a property the sanitiser can have:
        // replacing the hyphen is the entire point.
        //
        // So the guarantee is not "ids never collide" but "**a document containing a collision is
        // refused rather than turned into a client with two methods of one name**". In a
        // generated class the second method silently replaces the first, and half the client's
        // surface is unreachable with nothing in the package to say so. The refusal in
        // [`operations`] is the safety property, and it is the one that can be true.
        assert_eq!(
            RouteEntry::new("GET", "/step-up").operation_id(),
            RouteEntry::new("GET", "/step_up").operation_id()
        );
        let colliding = json!({
            "info": { "version": "v1" },
            "paths": {
                "/step-up": { "get": { "operationId": "get_step_up", "summary": "hyphen", "tags": ["x"] } },
                "/step_up": { "get": { "operationId": "get_step_up", "summary": "underscore", "tags": ["x"] } }
            }
        });
        let error = operations(&colliding).expect_err("a document that merges two routes is refused");
        assert!(error.to_string().contains("twice"), "{error}");

        // And the generator refuses it too, rather than emitting the half-reachable client.
        let hash = pinned(&colliding);
        let error = generate(&colliding, Language::TypeScript, &hash)
            .expect_err("no client is published from a document with a collision");
        assert!(error.to_string().contains("twice"), "{error}");
    }

    #[test]
    fn the_live_snapshot_has_no_such_collision() {
        // The refusal above is only worth having if the platform's own document passes it. Read
        // the committed snapshot rather than a fixture, because a fixture is written to agree
        // with the code under test and this is the one place the two must be independent.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../api/openapi.snapshot.json");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let document: Value = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let ops = operations(&document).expect("the committed snapshot has no duplicate ids");
        let mut seen = std::collections::BTreeSet::new();
        for op in &ops {
            assert!(
                seen.insert(op.id.clone()),
                "`{}` appears twice in the committed snapshot",
                op.id
            );
        }
        // Every id is usable as a method name in both languages, which is the property the whole
        // sanitiser exists to provide.
        for op in &ops {
            assert!(
                op.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "`{}` is not a legal identifier",
                op.id
            );
        }
    }
}
