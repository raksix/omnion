//! The OpenAPI document must not drift from the router it describes (REQ-033, slice 2).
//!
//! The request file's acceptance criterion is: *"The API Explorer lists operations from the
//! served OpenAPI document, and a CI check fails when the document drifts from the running
//! router."* This file is that check.
//!
//! # Why it parses the source rather than asking axum
//!
//! axum's `Router` exposes no route iterator, so the alternative would be a hard-coded list of
//! every path — which is a *second* hand-written table, and the exact thing that drifts. Parsing
//! `routes/mod.rs` means the truth is the router's own source: a route that is renamed, moved or
//! deleted changes this test's answer without anybody remembering to update a list.
//!
//! The parse is deliberately shallow. It reads `.route("<path>", <binding>)` and works out the
//! methods from the binding:
//!
//! * a bare name (`api_keys`) means "whatever that `let` bound", which the file resolves by
//!   looking up the `let` — and a `let` that merges a `get` with a `post` is read as both;
//! * an inline `get(...)` / `post(...)` / `put(...)` / `patch(...)` / `delete(...)` means that
//!   one method;
//! * anything it cannot resolve is reported as an **unresolved binding** rather than skipped.
//!
//! That last point is the load-bearing one. A parser that silently skipped what it could not
//! read would pass on a route it never saw, which is the same class of defect as an assertion
//! that cannot fail: this test fails when its own coverage drops below a stated floor, so a
//! future route written in a style the parser does not know shows up as a red test rather than
//! as a quiet hole.
//!
//! # What the test does NOT claim
//!
//! It compares `(method, path)` pairs, and it does not check that the permission in the
//! document is the permission the guard checks — that would need a walk of the `let` bindings
//! for `guards::require`, which is a different and much more brittle parse. The unit test
//! `every_documented_permission_is_one_the_catalogue_knows` covers the half that would be a
//! `403` for everybody, and the *live* behaviour (a call the caller cannot make answers `403`)
//! is proved by the Explorer run in the QA pass.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// The router source this test reads.
const ROUTER_SOURCE: &str = "src/routes/mod.rs";

/// The API prefix every mounted route lives under.
const PREFIX: &str = "/api/v1";

/// The five verbs axum's `routing` helpers build.
const METHODS: &[&str] = &["get", "post", "put", "patch", "delete"];

/// The floor on how many routes the parse is expected to see.
///
/// **Measured, not guessed.** At slice 2 the parse sees 260 `(method, path)` pairs from 256
/// `.route()` call sites — the extra four are the bindings that merge two verbs onto one path
/// (`/api-keys` carries a `get` and a `post`, `/api-keys/{id}` a `get` and a `delete`, and two
/// more). 250 is therefore the count with a small margin for a legitimate addition, and the
/// assertion is a **tripwire** rather than a coverage target: if a future change makes the
/// parser understand fewer routes than this, the test goes red instead of passing over a
/// smaller world. A parser that quietly stopped following a `Router::new()` block, or that met
/// a route written in a style it does not understand, would otherwise make every drift
/// assertion in this file true over a subset of the router — which is the same failure as an
/// assertion that cannot fail.
///
/// Raise it when a slice documents more of the application; never lower it to make a red test
/// go green without first reading what stopped being parsed.
const MINIMUM_ROUTES_SEEN: usize = 250;

#[test]
fn every_documented_operation_is_a_route_the_router_mounts() {
    let mounted = mounted_routes();
    let documented: BTreeSet<(String, String)> = omnion_developer::openapi::OPERATIONS
        .iter()
        .map(|operation| (operation.method.to_owned(), operation.path.to_owned()))
        .collect();

    let missing: Vec<&(String, String)> = documented
        .iter()
        .filter(|(method, path)| !mounted.contains(&(method.clone(), path.clone())))
        .collect();

    assert!(
        missing.is_empty(),
        "the document describes {} operation(s) the router does not mount — an operation the \
         Explorer would send and the API would 404: {missing:?}",
        missing.len()
    );
}

#[test]
fn every_mounted_route_is_documented_or_explicitly_excused() {
    let mounted = mounted_routes();
    let documented: BTreeSet<(String, String)> = omnion_developer::openapi::OPERATIONS
        .iter()
        .map(|operation| (operation.method.to_owned(), operation.path.to_owned()))
        .collect();
    let baseline: BTreeSet<(String, String)> = omnion_developer::openapi::UNDOCUMENTED_BASELINE
        .iter()
        .filter_map(|entry| entry.split_once(' '))
        .map(|(method, path)| (method.to_owned(), path.to_owned()))
        .collect();

    let undocumented: Vec<&(String, String)> = mounted
        .iter()
        .filter(|entry| !documented.contains(*entry) && !baseline.contains(*entry))
        .collect();

    assert!(
        undocumented.is_empty(),
        "{} mounted route(s) are neither documented nor in UNDOCUMENTED_BASELINE — document them \
         or add them to the baseline with a reason: {undocumented:?}",
        undocumented.len()
    );
}

#[test]
fn the_baseline_holds_no_route_that_is_gone() {
    // The direction that matters: a stale exemption is how a "no untested screen" rule quietly
    // stops testing. Removing a route must remove its exemption in the same commit.
    let mounted = mounted_routes();
    let stale: Vec<&&str> = omnion_developer::openapi::UNDOCUMENTED_BASELINE
        .iter()
        .filter(|entry| {
            entry
                .split_once(' ')
                .map(|(method, path)| !mounted.contains(&(method.to_owned(), path.to_owned())))
                .unwrap_or(true)
        })
        .collect();

    assert!(
        stale.is_empty(),
        "UNDOCUMENTED_BASELINE lists {} route(s) the router no longer mounts: {stale:?}",
        stale.len()
    );
}

#[test]
fn the_parse_still_sees_the_whole_router() {
    // The tripwire. A parser that quietly stopped following a `Router::new()` block would make
    // every test above pass over a smaller world, and the pass would be a lie.
    let mounted = mounted_routes();
    assert!(
        mounted.len() >= MINIMUM_ROUTES_SEEN,
        "the route parse saw only {} mounted routes; it used to see at least {MINIMUM_ROUTES_SEEN}. \
         A route written in a style this parser does not understand is a hole in the drift check.",
        mounted.len()
    );
}

#[test]
fn the_parse_reports_nothing_it_could_not_resolve() {
    // Every `.route(` in the router names a binding. A binding this parser cannot turn into a
    // method is a route the drift check does not know about, so it is named here rather than
    // skipped.
    let unresolved = unresolved_bindings();
    assert!(
        unresolved.is_empty(),
        "the route parse could not resolve {} binding(s): {unresolved:?}",
        unresolved.len()
    );
}

// ---------------------------------------------------------------------------------------------
// The parser
// ---------------------------------------------------------------------------------------------

/// Every `(method, path)` the router mounts under `/api/v1`.
fn mounted_routes() -> BTreeSet<(String, String)> {
    // The `let` bindings are read from the WHOLE file and only the `.route()` calls are
    // scoped to the `v1` block. Slicing the source first and then looking for the bindings is
    // backwards: axum's router is built by binding every handler *before* assembling the
    // router, so every name a route refers to is declared above the marker. Reading only the
    // slice finds no bindings at all and reports the entire router as unresolved — a parse
    // that looks for the truth in the one place the truth is not.
    let source = read_router_source();
    let lets = let_bindings(&source);
    let mut routes = BTreeSet::new();
    for (path, binding) in route_calls(&source) {
        for method in methods_of(&binding, &lets) {
            routes.insert((method, format!("{PREFIX}{path}")));
        }
    }
    routes
}

/// The `.route("<path>", <binding>)` calls **inside the versioned router**.
///
/// Only the `v1` block is read, and that is not a convenience: `/healthz` and `/readyz` are
/// mounted on the outer router precisely so a probe does not depend on the API version, and
/// prefixing them with `/api/v1` would put two paths in the document that no client can reach
/// and that the Explorer's own `prepare()` refuses.
///
/// The block is delimited on **both** sides — from `let v1 = Router::new()` to the outer
/// `Router::new()` that starts the application router. Slicing only from the start marker runs
/// to the end of the file and swallows `/healthz` and `/readyz` as `/api/v1/healthz` and
/// `/api/v1/readyz`: two invented paths that the drift check would then hold the document to,
/// and that a developer copying out of the Explorer would discover as a `404`.
fn route_calls(source: &str) -> Vec<(String, String)> {
    let Some(start) = source.find("let v1 = Router::new()") else {
        return Vec::new();
    };
    let rest = &source[start..];
    let end = rest
        .find("Router::new()\n        .route(\"/healthz\"")
        .unwrap_or(rest.len());
    let body = &rest[..end];

    let mut out = Vec::new();
    let mut from = 0;
    while let Some(found) = body[from..].find(".route(") {
        // The call is `.route("path", binding)`, but the router also writes it across lines:
        //
        // ```text
        // .route(
        //     "/iam/permissions",
        //     get(iam::list_permissions).layer(..),
        // )
        // ```
        //
        // and the path does not have to be on the line the call starts on. So the parse takes
        // everything up to the first `,` at paren-depth one — not the first `,` in the text,
        // which would be a comma inside a `guards::require(&state, "…")` on the same line.
        let open = from + found + ".route(".len();
        let Some(end_of_call) = matching_paren(body, open - 1) else {
            break;
        };
        let arguments = &body[open..end_of_call];
        let Some(comma) = top_level_comma(arguments) else {
            from = end_of_call;
            continue;
        };
        let path = arguments[..comma].trim().trim_matches('"').to_owned();
        let binding = arguments[comma + 1..]
            .trim_start()
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect::<String>();
        out.push((path, binding));
        from = end_of_call;
    }
    out
}

/// The index of the `)` that closes the `(` at `open`.
///
/// A naive "next `)`" reads the one inside `guards::require(&state, "x")` and returns an
/// argument list cut in half — which is a *parse* failure, and a parse failure is the dangerous
/// kind here because it silently drops the route instead of reporting it.
fn matching_paren(source: &str, open: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut depth = 0i32;
    let mut in_string = false;
    for (offset, byte) in bytes.iter().enumerate().skip(open) {
        let character = *byte as char;
        if in_string {
            if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(offset);
                }
            }
            _ => {}
        }
    }
    None
}

/// The first comma at paren depth zero, ignoring commas inside `(…)` and inside a string.
fn top_level_comma(arguments: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_string = false;
    for (offset, byte) in arguments.as_bytes().iter().enumerate() {
        let character = *byte as char;
        if in_string {
            if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => return Some(offset),
            _ => {}
        }
    }
    None
}

/// The methods a binding carries.
///
/// Two shapes, and the first one is a plain function call that happens to be named after a
/// verb: a binding written `post(handler)` *is* the method, while a binding written
/// `backups_create` is a `let` whose expression names the methods. A name that is also a verb
/// (`get`, `post`) can only be the inline form, because the router never binds a `let` to a
/// variable called `get`.
fn methods_of(binding: &str, lets: &BTreeMap<String, String>) -> Vec<String> {
    let mut methods = BTreeSet::new();
    if METHODS.contains(&binding) {
        methods.insert(binding.to_ascii_uppercase());
        return methods.into_iter().collect();
    }
    let Some(expression) = lets.get(binding) else {
        return vec!["UNRESOLVED".to_owned()];
    };
    collect_methods(expression, &mut methods);
    if methods.is_empty() {
        methods.insert("UNRESOLVED".to_owned());
    }
    methods.into_iter().collect()
}

/// Every verb named inside a binding expression.
///
/// Looks for `get(`, `post(` and friends anywhere in the expression, which handles
/// `get(h).merge(post(h2))` and a `.layer(...)` chain with the same code. The anchor on a
/// non-identifier character before the name is what keeps `iam::forget(` from reading as a
/// `get` — a substring search alone would find the tail of any word ending in a verb.
fn collect_methods(expression: &str, out: &mut BTreeSet<String>) {
    let bytes = expression.as_bytes();
    for method in METHODS {
        let needle = format!("{method}(");
        let mut from = 0;
        while let Some(found) = expression[from..].find(&needle) {
            let at = from + found;
            let before_is_boundary =
                at == 0 || !(bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_');
            if before_is_boundary {
                out.insert((*method).to_ascii_uppercase());
            }
            from = at + needle.len();
        }
    }
}

/// The `let <name> = <expression>` bindings of the router file.
///
/// # The type annotation, and why it is the third thing this parser has to handle
///
/// axum's bindings are frequently *typed*:
///
/// ```text
/// let media_raw: MethodRouter<AppState, Infallible> =
///     get(media_transform::raw_with_preset).layer(..);
/// ```
///
/// Splitting the name off at the first `=` therefore yields `media_raw: MethodRouter<AppState,
/// Infallible> ` as the "name" — which matches no binding and reports every typed route as
/// unresolved. There are three ways this router writes a `let` (one line, value on the next,
/// and split across several balanced by parentheses) and two ways it prefixes the name (bare,
/// and with a type annotation); missing any one of the five makes the parse silently smaller
/// than the router, which is exactly what the tripwire test below exists to catch.
///
/// The expression is read across **continuation lines**, and the condition for reading another
/// line is two-part: unbalanced parentheses, or an expression that is still empty because the
/// `=` ended the line. The second case is the one that breaks a depth-only reader — the first
/// line ends with `=` and balanced parens, so depth is zero and the binding resolves to the
/// empty string.
fn let_bindings(source: &str) -> BTreeMap<String, String> {
    let lines: Vec<&str> = source.lines().collect();
    let mut out = BTreeMap::new();
    for (number, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("let ") else {
            continue;
        };
        let Some((name, first)) = rest.split_once('=') else {
            continue;
        };
        // The declared name is what comes before a `:` annotation, trimmed. A name that is not
        // a plain identifier is a `let` destructuring or a pattern, and the router has none —
        // so it is skipped rather than guessed at.
        let name = name.split(':').next().unwrap_or("").trim().to_owned();
        if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        let mut expression = first.trim().to_owned();
        let mut lookahead = number + 1;
        loop {
            // Continuation has three conditions, and all three are load-bearing — the router
            // writes every one of these shapes:
            //
            // 1. **unbalanced parentheses** — `let x = get(h)\n    .merge(post(h2));`
            // 2. **an empty expression** — `let x =\n    get(h);` (the `=` ended the line, and
            //    the first line is *balanced*, so a depth test alone stops here)
            // 3. **the next line continues a chain** — which is the one a depth count cannot
            //    see, and it is the most common shape in the file:
            //
            // ```text
            // let api_keys = get(developer::list_keys)
            //     .layer(guards::require(&state, "developer.keys.read"))
            //     .merge(post(developer::create_key).layer(..));
            // ```
            //
            // Every line there is balanced, so a depth-only reader stops after the first and
            // reads `api_keys` as a `GET` alone. The merged `POST` becomes invisible — and the
            // failure is not merely a missing document entry: a `DELETE` merged the same way
            // would make the drift check *confirm* a verb the router does not serve on that
            // path, which is a false green on the one property the check exists to hold.
            //
            // So the third condition looks at the **next** line rather than at a tail
            // character: a line that starts with `.` is a continuation, always. That is a
            // stronger statement than "the last character is a dot" and it is the one the
            // formatter's output actually satisfies.
            let depth =
                expression.matches('(').count() as i32 - expression.matches(')').count() as i32;
            let next_line = lines
                .get(lookahead)
                .map(|line| line.trim())
                .unwrap_or_default();
            let continues = expression.is_empty()
                || depth > 0
                || next_line.starts_with('.')
                || next_line.starts_with(')');
            if continues {
                let Some(next) = lines.get(lookahead) else {
                    break;
                };
                expression.push(' ');
                expression.push_str(next.trim());
                lookahead += 1;
            } else {
                break;
            }
        }
        out.insert(name, expression);
    }
    out
}

/// Bindings a `.route()` names that the parser could not turn into a method.
fn unresolved_bindings() -> Vec<String> {
    let source = read_router_source();
    let lets = let_bindings(&source);
    let mut out = Vec::new();
    for (path, binding) in route_calls(&source) {
        if methods_of(&binding, &lets)
            .iter()
            .any(|m| m == "UNRESOLVED")
        {
            out.push(format!("{path} -> {binding}"));
        }
    }
    out
}

/// The router source, with the router's own comments stripped.
///
/// A comment mentioning a route — and this file is heavily commented about its routes — is not a
/// route. Stripping comments before parsing is what keeps a well-documented router from reading
/// as hundreds of phantom mounts.
fn read_router_source() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(ROUTER_SOURCE);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{} must be readable: {error}", path.display()));
    strip_comments(&raw)
}

/// Remove `//` line comments, leaving string literals alone.
///
/// A route path never contains `//`, so the simple rule is safe here: a `//` inside a string
/// would only appear in a URL, and the only such path in the router is `/public/...`, which has
/// no double slash. A `//` that *is* a comment is stripped, including the ones inside the block
/// comments' text, which is the intent.
fn strip_comments(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut in_string = false;
    let mut in_block = false;
    let bytes = source.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let two = source.get(index..index + 2);
        if in_block {
            if two == Some("*/") {
                in_block = false;
                index += 2;
                continue;
            }
            index += 1;
            continue;
        }
        if in_string {
            if bytes[index] == b'"' {
                in_string = false;
            }
            out.push(bytes[index] as char);
            index += 1;
            continue;
        }
        if two == Some("//") {
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if two == Some("/*") {
            in_block = true;
            index += 2;
            continue;
        }
        if bytes[index] == b'"' {
            in_string = true;
        }
        out.push(bytes[index] as char);
        index += 1;
    }
    out
}
