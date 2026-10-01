//! Instants on the wire.
//!
//! Every panel date cell is fed by an `OffsetDateTime` that crossed a serialiser, and this
//! suite is about the one place that conversion can quietly go wrong in a way no compiler
//! and no type mentions.
//!
//! **The defect.** `time`'s `Serialize` for `OffsetDateTime` has two arms. When the
//! serializer is human-readable it writes a formatted string — but only if the crate's
//! **`serde-human-readable`** feature is on. Without it the type falls through to a
//! **nine-element tuple**: year, ordinal, hour, minute, second, nanosecond, and three offset
//! parts. The workspace declares `serde-well-known`, which enables `serde`, `formatting` and
//! `parsing` but *not* `serde-human-readable`, so the string arm is compiled out and the
//! array arm is what actually ships.
//!
//! **Why it is silent.** Nothing above it objects. The admin declares `expires_at: string`,
//! so `tsc` is green; `formatTimestamp` guards with `Number.isNaN` and returns `"—"`. A
//! share link that expires renders no expiry, a scan run shows no time, a delivery shows no
//! attempt — and every one of them looks like a *design* decision rather than a bug, because
//! the fallback is exactly what a deliberate "nothing to show" also looks like. `new
//! Date([2026, 273, …])` is `Invalid Date`, so the guard fires and swallows it.
//!
//! The same applies in reverse: a bare `Option<OffsetDateTime>` on a **query** struct
//! *rejects* the RFC 3339 string every HTTP client sends, so a date filter is a 400 rather
//! than a filter. Proved below in both directions.
//!
//! Three things are checked, and the order matters — the cheapest, broadest check runs first:
//!
//! 1. **A real wire round trip** of the actual body types, asserting the JSON is a *string*.
//!    This is the ground truth; a source scan can only ever be a proxy for it.
//! 2. **Every serialised struct in the route modules is annotated.** A new `...Body` struct
//!    with a bare instant passes the compiler, passes `tsc`, and renders a dash — so the
//!    gate scans the sources and names the file and line.
//! 3. **The scan sees the code.** A walk that finds zero structs proves nothing, so the
//!    counts are asserted the way `tests/events.rs` asserts its own.

use omnion_api::routes;
use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

/// A stand-in for a real instant: a fixed day, a fixed hour, and a whole-second offset, so
/// the assertions below compare against a literal rather than against "something plausible".
const SAMPLE: &str = "2026-09-30T02:00:00Z";

fn sample() -> OffsetDateTime {
    OffsetDateTime::parse(SAMPLE, &time::format_description::well_known::Rfc3339)
        .expect("the sample instant parses")
}

/// The wire form every panel date must take: a JSON **string**.
///
/// `None` is checked separately, because `null` is the *correct* answer for an instant that
/// has not happened yet — a run still going, a share never revoked. The failure this guards
/// is the array, not the null: a null renders as an em dash because there is genuinely
/// nothing to show, and an array renders as the same em dash because the value was lost. Two
/// different causes, one identical pixel, which is why they need two different assertions.
fn assert_instant_wire(value: &Value, field: &str, owner: &str) {
    let raw = &value[field];
    let text = raw.as_str().unwrap_or_else(|| {
        panic!(
            "{owner}.{field} serialised as {raw} — a nine-element array, not a timestamp. \
             The panel reads that as `Invalid Date` and renders `—`, so the field is silently \
             absent from the screen. Annotate it with `#[serde(with = \
             \"time::serde::rfc3339\")]` (`::option` for an `Option<OffsetDateTime>`)."
        )
    });
    assert!(
        text.starts_with("2026-09-30T02:00:00"),
        "{owner}.{field} serialised as {text:?}; the sample instant lost its value"
    );
}

/// The other half of the instant contract: an instant that has not happened is `null`, and
/// the field is still *present* as null. A body that dropped the key entirely would make the
/// panel's `share.revoked_at` access undefined, which is a different bug with the same
/// symptom.
fn assert_null_wire(value: &Value, field: &str, owner: &str) {
    assert_eq!(
        value.get(field),
        Some(&Value::Null),
        "{owner}.{field} should be null when the instant has not happened; it is {}. \
         A missing key is not the same as a null one — the panel distinguishes them.",
        value
            .get(field)
            .unwrap_or(&Value::String("<absent>".to_owned()))
    );
}

/// The backup centre's four bodies between them carry every instant the panel shows on that
/// screen: a required one, an optional one, and one that is optional *and* null while a run
/// is still going.
#[test]
fn the_backup_bodies_answer_timestamps_as_strings() {
    let at = sample();

    let run = serde_json::to_value(routes::backups::BackupBody {
        id: Uuid::nil(),
        organization_id: None,
        label: "nightly".to_owned(),
        kind: "full".to_owned(),
        schedule_id: None,
        scopes: vec![],
        status: "succeeded".to_owned(),
        size_bytes: 1,
        destination: "local".to_owned(),
        storage_prefix: String::new(),
        checksum: None,
        protected: false,
        retain_until: Some(at),
        error: None,
        created_by: None,
        created_at: at,
        started_at: Some(at),
        finished_at: None,
        title: "nightly".to_owned(),
    })
    .expect("the run body serialises");
    for field in ["retain_until", "created_at", "started_at"] {
        assert_instant_wire(&run, field, "BackupBody");
    }
    for field in ["finished_at"] {
        assert_null_wire(&run, field, "BackupBody");
    }
    // `finished_at` is null here, and null is the right answer for a run that has not stopped.
    // The point is that it is null - not absent, and not a nine-element array.
    assert_eq!(run["finished_at"], Value::Null);

    let status = serde_json::to_value(routes::backups::StatusBody {
        last_successful_at: Some(at),
        last_successful_id: Some(Uuid::nil()),
        last_successful_age_seconds: Some(0),
        total_size_bytes: 1,
        counts: omnion_backup::StatusTotals {
            queued: 0,
            running: 0,
            succeeded: 1,
            partial: 0,
            failed: 0,
        },
        protected: 0,
        next_scheduled_at: Some(at),
        destination: routes::backups::DestinationBody {
            kind: "local".to_owned(),
            local_root: "/tmp".to_owned(),
            s3_prefix: None,
            credential_ref: None,
            writable: true,
            reason: String::new(),
            message: String::new(),
            encryption: "none".to_owned(),
        },
    })
    .expect("the status body serialises");
    // The card at the top of the screen reads both of these.
    for field in ["last_successful_at", "next_scheduled_at"] {
        assert_instant_wire(&status, field, "StatusBody");
    }

    let settings = serde_json::to_value(routes::backups::SettingsBody {
        destination: "local".to_owned(),
        local_root: "/tmp".to_owned(),
        s3_prefix: None,
        credential_ref: None,
        encryption: "none".to_owned(),
        default_retention: 7,
        verify_after_backup: true,
        updated_at: at,
    })
    .expect("the settings body serialises");
    assert_instant_wire(&settings, "updated_at", "SettingsBody");

    let schedule = serde_json::to_value(routes::backups::ScheduleBody {
        id: Uuid::nil(),
        organization_id: None,
        name: "nightly".to_owned(),
        frequency: "daily".to_owned(),
        at_time: Some("02:00".to_owned()),
        day_of_week: None,
        day_of_month: None,
        timezone: "Europe/Istanbul".to_owned(),
        scopes: vec![],
        retention_count: 7,
        destination: "local".to_owned(),
        enabled: true,
        last_run_at: Some(at),
        next_run_at: Some(at),
        last_backup_id: None,
        cadence: "daily".to_owned(),
    })
    .expect("the schedule body serialises");
    // `next_run_at` is the cell the last two slices of REQ-013 exist to fill. If it crosses
    // the wire as an array, the schedule table shows a dash next to a cadence sentence.
    for field in ["last_run_at", "next_run_at"] {
        assert_instant_wire(&schedule, field, "ScheduleBody");
    }
}

/// A panel type declares these as `string`, and TypeScript cannot check what a serialiser
/// did - which is exactly why the media bodies are walked separately rather than assumed.
#[test]
fn the_media_bodies_answer_timestamps_as_strings() {
    let at = sample();

    let share = serde_json::to_value(routes::media_shares::ShareBody {
        id: Uuid::nil(),
        media_id: Uuid::nil(),
        expires_at: Some(at),
        has_password: false,
        download_count: 0,
        created_at: at,
        revoked_at: None,
        revoked_reason: String::new(),
        state: "live",
    })
    .expect("the share body serialises");
    for field in ["expires_at", "created_at"] {
        assert_instant_wire(&share, field, "ShareBody");
    }
    for field in ["revoked_at"] {
        assert_null_wire(&share, field, "ShareBody");
    }
    assert_eq!(share["revoked_at"], Value::Null);

    let run = serde_json::to_value(routes::media_retention::RunBody {
        id: Uuid::nil(),
        kind: "prune".to_owned(),
        versions_removed: 0,
        versions_bytes: 0,
        purged: 0,
        purged_bytes: 0,
        refused: 0,
        held_back: 0,
        error: String::new(),
        started_at: at,
        finished_at: None,
        summary: "nothing to do".to_owned(),
        bytes_reclaimed: 0,
    })
    .expect("the retention run serialises");
    for field in ["started_at"] {
        assert_instant_wire(&run, field, "RunBody");
    }
    for field in ["finished_at"] {
        assert_null_wire(&run, field, "RunBody");
    }

    let quarantine = serde_json::to_value(routes::media_scan::QuarantineBody {
        id: Uuid::nil(),
        media_id: Uuid::nil(),
        detail: "could not be classified".to_owned(),
        quarantined_at: at,
        run_id: None,
    })
    .expect("the quarantine body serialises");
    assert_instant_wire(&quarantine, "quarantined_at", "QuarantineBody");

    let scan = serde_json::to_value(routes::media_scan::ScanRunBody {
        id: Uuid::nil(),
        kind: "scan".to_owned(),
        outcome: "succeeded".to_owned(),
        scanned: 0,
        flagged: 0,
        errors: 0,
        skipped: 0,
        endpoint: "local".to_owned(),
        engine: "clamav".to_owned(),
        started_at: at,
        finished_at: None,
        summary: "nothing scanned".to_owned(),
    })
    .expect("the scan run serialises");
    for field in ["started_at"] {
        assert_instant_wire(&scan, field, "ScanRunBody");
    }
    for field in ["finished_at"] {
        assert_null_wire(&scan, field, "ScanRunBody");
    }

    let recent = serde_json::to_value(routes::commands::RecentItemBody {
        kind: "query",
        query: Some("invoices".to_owned()),
        command_id: None,
        title: None,
        route: Some("/pages".to_owned()),
        result_count: Some(4),
        created_at: at,
    })
    .expect("the recent item serialises");
    assert_instant_wire(&recent, "created_at", "RecentItemBody");
}

/// The reverse direction, and the sharper half of it.
///
/// A bare `OffsetDateTime` on a query struct **rejects** the string every client sends, so a
/// date filter is a 400 instead of a filter. That is the failure an operator notices, because
/// they typed a date and it "did not work".
///
/// The second half is the one that nearly shipped in this very commit. Adding
/// `#[serde(with = "...::option")]` to a query field makes serde **require the key to be
/// present**: `{"created_after": ...}` parses, `{}` fails with `missing field created_after`.
/// The obvious request — a filter that was not supplied — is exactly the shape of an ordinary
/// "list everything" call, so the annotation turned every unfiltered list into a 400, and the
/// *absence* case is the one that catches it. `default` is load-bearing, not decoration.
///
/// The key is snake_case because that is what `axum::extract::Query` deserialises from a query
/// string, and this test writes the JSON the way the extractor builds it. Writing camelCase
/// here is a silent pass: an unmatched key is ignored, the field stays `None`, and the
/// assertion "the filter parsed" passes for a filter that was never applied.
#[test]
fn a_query_struct_accepts_both_the_instant_a_client_sends_and_its_absence() {
    let query: routes::backups::ListQuery = serde_json::from_value(serde_json::json!({
        "created_after": SAMPLE,
        "created_before": "2026-10-01T00:00:00Z",
    }))
    .expect("a query filter parses the RFC 3339 string a client sends");
    assert_eq!(query.created_after, Some(sample()));
    assert_eq!(
        query.created_before,
        OffsetDateTime::parse(
            "2026-10-01T00:00:00Z",
            &time::format_description::well_known::Rfc3339
        )
        .ok()
    );

    // The unfiltered list: every key absent. This must NOT be an error.
    let unfiltered: routes::backups::ListQuery =
        serde_json::from_value(serde_json::json!({})).expect("an unfiltered list still parses");
    assert_eq!(unfiltered.created_after, None);
    assert_eq!(unfiltered.created_before, None);

    let files: routes::media_files::FileQuery = serde_json::from_value(serde_json::json!({
        "site_id": Uuid::nil(),
        "created_after": SAMPLE,
    }))
    .expect("the file query parses the same string");
    assert_eq!(files.created_after, Some(sample()));

    let unfiltered_files: routes::media_files::FileQuery =
        serde_json::from_value(serde_json::json!({ "site_id": Uuid::nil() }))
            .expect("the unfiltered file list still parses");
    assert_eq!(unfiltered_files.created_after, None);

    // A camelCase key does NOT bind, and the result is a silently absent filter rather than an
    // error. Asserting the snake_case form above is therefore load-bearing in a second way:
    // it is the only thing that distinguishes "parsed" from "parsed into nothing".
    let camel: routes::backups::ListQuery = serde_json::from_value(serde_json::json!({
        "createdAfter": SAMPLE,
    }))
    .expect("an unmatched key is ignored rather than refused");
    assert_eq!(
        camel.created_after, None,
        "this struct does not rename to camelCase; the filter above is bound under the \
         snake_case name"
    );

    // And a malformed instant is refused by name rather than silently ignored.
    let bad = serde_json::from_value::<routes::backups::ListQuery>(serde_json::json!({
        "created_after": "last tuesday",
    }));
    assert!(
        bad.is_err(),
        "a nonsense instant must not parse into a filter"
    );
}

/// The gate for the gate. A bare instant compiles, passes `tsc` and renders a dash, so the
/// class needs a source-level check that names the file and the line — a new body struct is
/// exactly the moment this defect comes back.
#[test]
fn no_serialised_struct_carries_a_bare_instant() {
    let routes_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/routes");
    let entries = std::fs::read_dir(&routes_dir)
        .expect("the route modules are readable")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .collect::<Vec<_>>();
    assert!(
        entries.len() > 20,
        "the scan found {} route modules; a walk that sees nothing proves nothing",
        entries.len()
    );

    // The workspace is where the feature that decides the wire form is declared, and it is
    // the whole answer to "why does this happen at all": `serde-well-known` does NOT imply
    // `serde-human-readable`.
    let manifest_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
    let manifest = std::fs::read_to_string(&manifest_path).expect("the workspace manifest reads");
    assert!(
        !manifest.contains("\"serde-human-readable\""),
        "the workspace enables `time/serde-human-readable`, so the nine-element array arm is \
         compiled out and the per-field `#[serde(with = ...)]` attributes are no longer needed. \
         Delete them in the same commit that removes this check's findings — leaving them is \
         harmless, but leaving the assertion would be a lie."
    );

    let mut unannotated: Vec<String> = Vec::new();
    let mut bodies_seen = 0_usize;
    for path in &entries {
        let source = std::fs::read_to_string(path).expect("a route module reads");
        let relative = path
            .strip_prefix(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
            .unwrap_or(path)
            .display()
            .to_string();
        let lines: Vec<&str> = source.lines().collect();

        for (index, line) in lines.iter().enumerate() {
            let Some(name) = line.trim().strip_prefix("pub struct ").and_then(|rest| {
                rest.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    .next()
            }) else {
                continue;
            };
            if !name.ends_with("Body") && !name.ends_with("Query") && !name.ends_with("Response") {
                continue;
            }
            // Walk to the closing brace of the struct body.
            let mut depth = 1_i32;
            let mut cursor = index + 1;
            while cursor < lines.len() && depth > 0 {
                depth += lines[cursor].matches('{').count() as i32;
                depth -= lines[cursor].matches('}').count() as i32;
                cursor += 1;
            }
            bodies_seen += 1;

            for field in index + 1..cursor {
                let trimmed = lines[field].trim();
                if !trimmed.starts_with("pub ") || !trimmed.contains("OffsetDateTime") {
                    continue;
                }
                // Look back over the WHOLE attribute block, not just the previous line. A
                // one-line lookback is the version that added a second `#[serde(with = ...)]`
                // under a four-line `#[serde(with = ..., skip_serializing_if = ...)]`, which
                // does not fail this check - it fails the *build*, with `duplicate serde
                // attribute`, which is a worse way to learn it.
                let mut annotated = false;
                for back in (index..field).rev() {
                    let text = lines[back].trim();
                    if text.contains("rfc3339") {
                        annotated = true;
                        break;
                    }
                    // Stop as soon as the attribute block ends: a doc comment or a previous
                    // field means the lookback has walked past what it was looking for.
                    if text.starts_with("///") || text.starts_with("pub ") || text.is_empty() {
                        break;
                    }
                }
                if annotated {
                    continue;
                }
                unannotated.push(format!("{relative}:{} — {name}::{trimmed}", field + 1));
            }
        }
    }

    assert!(
        bodies_seen > 60,
        "the scan looked inside {bodies_seen} bodies; a scan that finds almost none is not \
         looking at the shapes that reach the panel"
    );
    assert!(
        unannotated.is_empty(),
        "{} serialised field(s) would cross the wire as a nine-element array, which the panel \
         reads as `Invalid Date` and renders `—`:\n  {}\n\nAnnotate each with \
         `#[serde(with = \"time::serde::rfc3339\")]` or `::option`.",
        unannotated.len(),
        unannotated.join("\n  ")
    );
}
