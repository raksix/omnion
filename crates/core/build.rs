//! Build script for the core crate.
//!
//! The migration files are embedded into every binary at compile time (`sqlx::migrate!` in
//! `src/db.rs`), and cargo has to be told they matter.
//!
//! ## Why this lists files instead of naming the directory
//!
//! This used to emit a single directive:
//!
//! ```text
//! cargo:rerun-if-changed=../../database/migrations
//! ```
//!
//! A *directory* path tells cargo to re-run when an entry is added or removed. It does **not**
//! tell cargo that the *content* of an existing entry changed, so editing a migration — which is
//! what a developer does when a check constraint is wrong, a column is renamed or an index is
//! rebuilt — leaves the compiled crate holding the old SQL. `include_str!` picks the new bytes up
//! on the next compile, while the macro's `no_tx` constant — computed from the file at expansion
//! time — does not, and the two halves disagree in exactly the way that is hardest to read: the
//! file on disk is right and the binary is wrong.
//!
//! That disagreement is not hypothetical. It is why
//! `crates/core/tests/migration_transaction_directive.rs` compares the *embedded* set against the
//! files, and why it can only be believed once this build script stops lying about the
//! dependency.
//!
//! Listing each file makes the dependency exact: cargo hashes them, so an edit re-runs the build
//! script, re-expands the macro, and the binary carries the same SQL a developer can read.

use std::path::Path;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let migrations = Path::new(&manifest_dir).join("../../database/migrations");

    println!("cargo:rerun-if-changed={}", migrations.display());

    // Read every entry, not just `.sql`: a directory whose contents change must be watched
    // closely enough that a new migration file is noticed even if the listing ever fails.
    let mut count = 0usize;
    if let Ok(entries) = std::fs::read_dir(&migrations) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("sql") {
                println!("cargo:rerun-if-changed={}", path.display());
                count += 1;
            }
        }
    }

    // An empty directory must be visible rather than a silent success: a worktree whose
    // `database/migrations` is missing or unreadable would otherwise embed an empty set and the
    // API would start with no schema and no complaint.
    if count == 0 {
        println!(
            "cargo:warning=omnion-core: no migration files found in {}",
            migrations.display()
        );
    }
}