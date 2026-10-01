//! The migration set the binary can actually install. (REQ-117 slice 21 defect, REQ-133 slice 18)
//!
//! ## The defect this exists for
//!
//! `0200_crm_intake_source_key_lookup.sql` shipped with `create index concurrently`. sqlx wraps
//! every migration in one transaction (`sqlx::migrate!`), `concurrently` is illegal inside a
//! transaction, and **every fresh installation failed to boot**:
//!
//! ```text
//! migration: while executing migration 200: CREATE INDEX CONCURRENTLY cannot run inside a
//! transaction block
//! ```
//!
//! It was found by a browser walkthrough whose API had died — four layers from the cause — and
//! the file's own comment had already predicted the error and then argued it was "the runner's
//! shape, not this migration's". A comment is not a boot path.
//!
//! ## What is asserted, and why each half is a separate question
//!
//! 1. **No migration executes a concurrent build.** Not "no migration does it wrongly" — none at
//!    all. A migration runs at install, before the platform serves traffic, so the write path a
//!    concurrent build would protect does not yet exist; the availability argument for it applies
//!    to a moment that has not arrived. The index built by a plain `create index` is the same
//!    index, so the protection buys nothing and costs the ability to install at all.
//!
//! 2. **A `-- no-transaction` directive, if one is ever written, is the file's FIRST BYTES.**
//!    sqlx computes `no_tx` as `sql.starts_with("-- no-transaction")` and bakes the answer into
//!    the binary at compile time. A directive below a comment header is present, greppable,
//!    correct-looking and **inert** — which is precisely the shape that hid the original defect.
//!    This test is what makes an inert directive impossible to write by accident.
//!
//! ## Why it reads the EMBEDDED set and not only the files
//!
//! "The file is right" and "the binary knows it" are two different facts, joined only by a
//! compile-time macro expansion. Reading the constant catches a stale expansion; reading the file
//! alone never will. `migration_flags` is that accessor, added for this test.

use omnion_core::db::migration_flags;

/// Files whose first bytes declare an exemption from the runner's transaction.
///
/// Empty is the correct and expected value: this branch's migration set has no legitimate reason
/// to leave the transaction, and a name appearing here means somebody has to argue for it. It is
/// an explicit allowlist rather than a derived one because the argument is a judgement — "this
/// index really does need to be built while the table is live" — and a judgement cannot be
/// derived from the SQL.
const OPT_OUT_EXPECTED: &[i64] = &[];

#[test]
fn no_migration_executes_a_concurrently_built_index() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../database/migrations");
    let offenders: Vec<String> = std::fs::read_dir(&root)
        .expect("the migration directory sits next to this crate")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().and_then(|e| e.to_str()) == Some("sql"))
        .filter(|entry| {
            let sql = std::fs::read_to_string(entry.path()).unwrap_or_default();
            // Comments are stripped first: 0200's own note discusses `create index concurrently`
            // at length, and a gate that cannot tell prose from a statement is a gate that will
            // be turned off.
            let code: String = sql
                .lines()
                .filter(|line| !line.trim_start().starts_with("--"))
                .collect::<Vec<_>>()
                .join("\n");
            let lowered = code.to_lowercase();
            lowered.contains("create index concurrently")
                || lowered.contains("create unique index concurrently")
                || lowered.contains("reindex concurrently")
        })
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();

    assert!(
        offenders.is_empty(),
        "these migrations execute a concurrent build, which sqlx refuses inside the runner's \
         transaction, so a fresh install cannot boot: {offenders:?}. A migration runs before the \
         platform serves traffic, so use a plain `create index`; if a concurrent build is truly \
         unavoidable, the file's FIRST BYTES must be `-- no-transaction`."
    );
}

#[test]
fn a_transaction_exemption_is_the_files_first_bytes_and_not_a_line_somewhere_inside_it() {
    // The distinction the previous test cannot make on its own: a `-- no-transaction` that is
    // present but not first is *inert*, and an inert directive reads exactly like a working one.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../database/migrations");
    for entry in std::fs::read_dir(&root).expect("the migration directory").filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("sql") {
            continue;
        }
        let sql = std::fs::read_to_string(&path).expect("a readable migration file");
        let declares = sql.lines().any(|line| line.trim() == "-- no-transaction");
        if declares && !sql.starts_with("-- no-transaction") {
            panic!(
                "{}: the `-- no-transaction` directive is not the file's first bytes, so sqlx \
                 never sees it (it tests `sql.starts_with`) and the install fails exactly as if \
                 it were absent. Put it on line 1 — above the file's comment header, not below \
                 it.",
                path.display()
            );
        }
    }
}

#[test]
fn the_embedded_set_matches_what_the_files_claim() {
    // The two halves of the same fact, checked against each other. A macro expansion that did
    // not re-run leaves a binary whose constant disagrees with the file on disk, and the file is
    // what a developer reads and edits — so the file is what the binary must be checked against.
    let flags = migration_flags();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../database/migrations");
    let files: Vec<i64> = std::fs::read_dir(&root)
        .expect("the migration directory")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().and_then(|e| e.to_str()) == Some("sql"))
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.split('_').next()?.parse::<i64>().ok()
        })
        .collect();

    assert_eq!(
        files.len(),
        flags.len(),
        "the binary embeds {} migrations and the directory holds {}. `sqlx::migrate!` expands at \
         compile time and cargo does not fingerprint `database/migrations/*.sql` — \
         `crates/core/build.rs` asks only to be re-run when the DIRECTORY changes, which a new \
         file does and an edit to an existing one does not. Touch `crates/core/src/db.rs` (or \
         `cargo clean -p omnion-core`) after editing a migration's *content*.",
        flags.len(),
        files.len()
    );

    let unexpected: Vec<i64> = flags
        .iter()
        .filter(|(_version, no_tx)| **no_tx)
        .map(|(version, _)| *version)
        .filter(|version| !OPT_OUT_EXPECTED.contains(version))
        .collect();
    assert!(
        unexpected.is_empty(),
        "these embedded migrations run outside the runner's transaction but are not expected to: \
         {unexpected:?}. An exemption is added to `OPT_OUT_EXPECTED` with its reason, never left \
         implicit."
    );
}