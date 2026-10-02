//! Write a real scaffold archive to disk, so a non-Rust reader can open it (REQ-033, slice 4).
//!
//! The unit tests in `archive.rs` round-trip through this crate's own reader. That catches a
//! writer and a reader that agree on a mistake and misses the mistakes they agree on, which
//! for a file format is the entire risk: a zip with a right payload and a wrong relative offset
//! reads fine in Rust and fails to extract in every real tool, on the developer's machine, after
//! they have left the platform.
//!
//! `scripts/qa/probe-scaffold-archive.py` drives this binary and opens the bytes with Python's
//! `zipfile` and with `/usr/bin/unzip`. This example is what makes that possible from the crate
//! itself rather than from a hand-written archive in a test fixture, which would prove only that
//! Python can read Python's fixture.
//!
//! It is an `example`, not a test and not a binary: `cargo test` does not build it, so it costs
//! nothing on a build that is not measuring the archive.

use std::io::Write as _;

use omnion_developer::archive::{entries_of, filename_for, zip};
use omnion_developer::scaffold::{ScaffoldKind, ScaffoldTarget};
use omnion_developer::templates::generate;

/// The name this example generates under. It is a slug, so it exercises the same rules a real
/// request would: a name with a space is refused, which is the point of the check upstream.
const NAME: &str = "plugin-probe";

fn main() {
    let scaffold = generate(ScaffoldKind::Plugin, NAME, ScaffoldTarget::Live)
        .expect("the plugin template generates");
    let archive = zip(&entries_of(&scaffold), now());

    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "qa-artifacts/scaffold-archive-probe".to_string());
    let dir = std::path::Path::new(&out);
    std::fs::create_dir_all(dir).expect("the output directory can be created");

    // The filename the platform would put in `Content-Disposition` is the filename the file
    // gets here, so the probe also checks that a name with the extension appended is the name
    // that opens.
    let path = dir.join(filename_for(&scaffold));
    let mut file = std::fs::File::create(&path).expect("the archive can be created");
    file.write_all(&archive.bytes)
        .expect("the archive can be written");
    file.sync_all().expect("the archive reaches the disk");

    // The path on stdout and nothing else: the probe reads it with `Path(stdout.strip())`, so a
    // progress line here would be parsed as a filename and fail in a way that looks like a
    // broken archive.
    println!("{}", path.display());
}

/// The instant stamped into the archive.
///
/// A fixed one rather than `now_utc()`, because the probe compares two runs byte for byte — a
/// clock-derived timestamp would make every run a "difference" and train whoever reads the
/// output to ignore it.
fn now() -> time::OffsetDateTime {
    time::OffsetDateTime::new_utc(
        time::Date::from_calendar_date(2026, time::Month::October, 2)
            .expect("a calendar date in range"),
        time::Time::from_hms(12, 0, 0).expect("a time in range"),
    )
}
