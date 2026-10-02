//! Generate the SDK packages from the committed OpenAPI document (REQ-130, slice 3).
//!
//! ```text
//! cargo run -p omnion-api --bin sdk_emit                       # both languages
//! cargo run -p omnion-api --bin sdk_emit -- --language python
//! cargo run -p omnion-api --bin sdk_emit -- --hash sha256:…   # require a specific pin
//! ```
//!
//! ## The document is read, never rebuilt
//!
//! Unlike `openapi_emit`, this binary does **not** build the router. It reads
//! `api/openapi.snapshot.json` — the same bytes `--check` compares against — and the hash it
//! prints is that file's own. The reason is that a generator which could see the router would be
//! able to emit a client for a route the committed document does not describe, and then the pin
//! on the release would be a hash of something nobody can reproduce from the repository.
//!
//! ## The pin is a gate, not a record
//!
//! `--hash` is how a release says *this exact API*. Without it the binary prints the hash and
//! writes; with it, a document that has moved under the pin is refused. Generation itself takes
//! the pin as an argument and checks it (see `omnion_graphql::sdk::generate`), so the guarantee
//! does not depend on which caller invoked it.

use std::path::PathBuf;
use std::process::ExitCode;

use omnion_graphql::sdk::{self, Language};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut languages: Vec<Language> = Vec::new();
    let mut pinned: Option<String> = None;
    let mut out_dir: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--language" | "-l" => {
                let Some(value) = args.get(i + 1) else {
                    return fail("--language needs a value (typescript or python)");
                };
                match Language::parse(value) {
                    Ok(language) => languages.push(language),
                    Err(e) => return fail(&e.to_string()),
                }
                i += 2;
            }
            "--hash" => {
                let Some(value) = args.get(i + 1) else {
                    return fail("--hash needs a value (sha256:…)");
                };
                pinned = Some(value.clone());
                i += 2;
            }
            "--out" => {
                let Some(value) = args.get(i + 1) else {
                    return fail("--out needs a directory");
                };
                out_dir = Some(PathBuf::from(value));
                i += 2;
            }
            other => return fail(&format!("unknown argument `{other}`")),
        }
    }
    if languages.is_empty() {
        languages = Language::all().to_vec();
    }

    let snapshot = omnion_api::openapi_emit::SNAPSHOT_PATH;
    let text = match std::fs::read_to_string(snapshot) {
        Ok(text) => text,
        Err(e) => {
            return fail(&format!(
                "{}: {e}. Run `cargo run -p omnion-api --bin openapi_emit` first.",
                snapshot
            ))
        }
    };
    let document: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(e) => return fail(&format!("{} is not valid JSON: {e}", snapshot)),
    };

    // The document's own hash, and the one every package is pinned to. Computed here rather than
    // read out of the file, because a hash stored *in* the thing it hashes is a value that can
    // disagree with itself — and the generator's whole claim is that the pin describes the bytes.
    let canonical = omnion_graphql::openapi::canonical_json(&document);
    let actual = omnion_graphql::openapi::Drift::openapi_hash(&canonical);
    if let Some(expected) = &pinned {
        if expected != &actual {
            return fail(&format!(
                "the snapshot hashes to {actual}, not the pinned {expected}; \
                 refusing to generate a client that would claim a provenance it does not have"
            ));
        }
    }
    eprintln!("sdk-emit: document {snapshot}");
    eprintln!("sdk-emit: hash {actual}");

    let root = out_dir
        .or_else(|| std::env::var("OMNION_SDK_OUT").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("dist/sdks"));

    let mut failed = false;
    for language in languages {
        match sdk::generate(&document, language, &actual) {
            Ok(package) => {
                let dir = root.join(language.as_str());
                if let Err(e) = write_package(&dir, &package) {
                    eprintln!("sdk-emit: FAILED to write {}: {e}", dir.display());
                    failed = true;
                    continue;
                }
                let bytes: usize = package.files.iter().map(|f| f.contents.len()).sum();
                eprintln!(
                    "sdk-emit: {} → {} ({} files, {} bytes, {} operations)",
                    language.as_str(),
                    dir.display(),
                    package.files.len(),
                    bytes,
                    sdk::operations(&document).map(|o| o.len()).unwrap_or(0),
                );
            }
            Err(e) => {
                eprintln!("sdk-emit: FAILED for {}: {e}", language.as_str());
                failed = true;
            }
        }
    }

    if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

/// Write a package, refusing to leave a half-written directory behind.
///
/// The refusal is the point. A generator that creates `dist/sdks/typescript/` and then fails
/// half way through leaves a directory that looks like a package to whatever runs next — a
/// publish step, a smoke test — and that package is missing files. Writing into a temporary
/// sibling and renaming means the destination either does not exist or is complete.
fn write_package(
    dir: &std::path::Path,
    package: &sdk::Package,
) -> std::io::Result<()> {
    let staging = dir.with_extension("staging");
    let _ = std::fs::remove_dir_all(&staging);
    for file in &package.files {
        let path = staging.join(&file.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, &file.contents)?;
    }
    let _ = std::fs::remove_dir_all(dir);
    std::fs::rename(&staging, dir)
}

fn fail(message: &str) -> ExitCode {
    eprintln!("sdk-emit: {message}");
    ExitCode::from(2)
}
