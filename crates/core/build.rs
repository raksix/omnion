//! Build script for the core crate.
//!
//! The migration files are embedded into every binary at compile time (`sqlx::migrate!` in
//! `src/db.rs`), and adding a file to `database/migrations` does not, by itself, invalidate the
//! compiled crate — a new migration would silently keep the old embedded set until something
//! else triggered a rebuild. Tracking the directory here makes the dependency explicit.

fn main() {
    println!("cargo:rerun-if-changed=../../database/migrations");
}
