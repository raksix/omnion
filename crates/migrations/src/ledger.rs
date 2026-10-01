//! The ledger: what this installation applied, from which bytes, and whether anybody has proved
//! the reversal works (docs/requests/REQ-129, slice 1).
//!
//! ## Why this is a table and not a query against `_sqlx_migrations`
//!
//! SQLx already records which versions ran. It cannot record **who** ran them, **from which
//! bytes**, or **whether the reversal was ever executed against a real database** — and those
//! three are the whole question an operator has when they are deciding whether to take a backup.
//! So this is a second table, reconciled with SQLx's rather than replacing it, and the runner
//! writes both. The reconciliation rule is one-directional on purpose: a ledger row is written
//! only after SQLx has committed the migration, so the ledger can never claim something the
//! database did not do. The other direction — a SQLx row with no ledger row — is a recoverable
//! gap and is what [`LedgerRow`]'s backfill reports.
//!
//! ## The checksum is over the bytes that ran
//!
//! Not over "the file as it is now". A checksum recomputed at read time from the working tree
//! agrees with itself forever and detects nothing, which is the failure the feature exists to
//! prevent. So [`checksum`] hashes the exact content the migration ran from, and comparing it is
//! how an edit to an applied migration becomes loud. There is deliberately no API that updates a
//! checksum: re-blessing an edited migration destroys the only evidence that the schema on disk
//! and the schema in the database were ever the same thing.
//!
//! ## Drift is not an error to be swallowed
//!
//! A drifted file means the source of truth and the database have parted company, and every
//! subsequent decision — including "which migration is next" — is made from one of them. So the
//! runner refuses before it applies anything, and the refusal names the file. See [`detect_drift`]
//! for the exact rule, which is the one thing here that is pure and therefore unit-tested
//! without a database.

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::OffsetDateTime;

use crate::error::{MigrationSafetyError, Result};

/// Where a migration run was started from. Mirrors the `source` check constraint on
/// `migration_runs`; the duplicate list is the price of not letting the database be the schema
/// definition for a Rust enum, and the walk proves the two agree by inserting every variant.
pub const SOURCES: [&str; 4] = ["cli", "deploy", "ci", "boot"];

/// sha256 of the file as applied, lowercase hex.
///
/// Content-addressed on purpose: the same file applied to two installations has the same
/// checksum, so a ledger comparison is a string comparison and an operator can verify one with
/// `sha256sum` without this tool.
#[must_use]
pub fn checksum(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    hex::encode(digest)
}

/// One row of the ledger, as the ledger screen reads it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, serde::Serialize)]
pub struct LedgerRow {
    /// `NNNN`.
    pub version: String,
    /// The name after the underscore.
    pub name: String,
    /// sha256 of the file as applied.
    pub checksum: String,
    pub applied_at: OffsetDateTime,
    pub duration_ms: i32,
    pub statement_count: i32,
    pub actor: String,
    pub source: String,
    pub has_down: bool,
    pub down_verified_at: Option<OffsetDateTime>,
    pub down_verified_by: Option<String>,
    pub waiver_reason: Option<String>,
}

/// What the runner wants to record about a migration it just applied.
#[derive(Debug, Clone)]
pub struct NewLedgerRow {
    /// `NNNN`.
    pub version: String,
    /// The name after the underscore.
    pub name: String,
    /// The checksum of the file that ran — [`checksum`] of the exact content, not of a
    /// re-derivation.
    pub checksum: String,
    pub duration_ms: i32,
    pub statement_count: i32,
    pub actor: String,
    pub source: String,
    /// Whether the file carries a down script. See [`extract_down`] for what "carries" means;
    /// it is deliberately conservative.
    pub has_down: bool,
    /// Set only when a waiver exists for this migration, copied onto the row so the ledger says
    /// why on its own.
    pub waiver_reason: Option<String>,
}

/// A migration the ledger disagrees with the files about.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Drift {
    /// The version whose checksum does not match.
    pub version: String,
    /// The name after the underscore.
    pub name: String,
    /// What the ledger recorded when the file was applied.
    pub recorded: String,
    /// What the file hashes to now.
    pub current: String,
}

impl Drift {
    /// The one-line message the runner refuses with, naming the offending file.
    ///
    /// It renders `0199_deployment_tooling.sql` — the literal filename — rather than the version
    /// and name as two fields, because the version is a lookup key while the thing an operator
    /// has to open is a file, and `grep 0199 database/migrations` finds it but `ls
    /// 0199_deployment_tooling.sql` is what you type. The two are also kept separately in the
    /// struct, so a screen can link to a row without parsing this message.
    ///
    /// The last clause is not decoration: it is the resolution, and the only resolution. An
    /// operator who reads "the checksum changed" and re-runs to see if it settles has been told
    /// nothing. Re-blessing an edited migration is what this feature exists to make impossible.
    #[must_use]
    pub fn message(&self) -> String {
        // The two cases render differently because they are different facts, not for symmetry:
        // a changed checksum has two hashes worth printing, a missing file has nothing to
        // compare against and saying "no longer matches" would be a lie about a value that was
        // never computed.
        let file = if self.current.is_empty() {
            format!("{}_{}.sql", self.version, self.name)
        } else {
            format!(
                "{}_{}.sql — applied as {}, hashes {} now",
                self.version, self.name, self.recorded, self.current
            )
        };
        format!(
            "{file}: an applied migration is immutable, so revert the file or add a new migration \
             that supersedes it; there is no way to re-bless an edited one"
        )
    }
}

/// Compare the ledger's recorded checksums against the files and report every mismatch.
///
/// Pure on purpose: it takes two already-read lists and answers one question, so it is unit
/// tested without a database and — more importantly — so it can be called from a `--dry-run`
/// that must not touch anything.
///
/// The comparison is one-directional by construction. A file with no ledger row is **pending**,
/// not drift: that is the normal state of a migration somebody just wrote. Only a ledger row with
/// no matching file, or a file whose hash moved, is drift — and the first case is how a deleted
/// file presents itself, which is why [`Drift::recorded`] and [`Drift::current`] both exist even
/// though the second one is meaningless for a deletion.
#[must_use]
pub fn detect_drift(
    ledger: &[(String, String, String)],
    files: &[(String, String, String)],
) -> Vec<Drift> {
    let mut drifts = Vec::new();
    for (version, name, recorded) in ledger {
        match files.iter().find(|(v, _, _)| v == version) {
            Some((_, _, current)) if current != recorded => drifts.push(Drift {
                version: version.clone(),
                name: name.clone(),
                recorded: recorded.clone(),
                current: current.clone(),
            }),
            // A ledger row with no file at all is drift too, and it is the one a checksum
            // comparison alone cannot find: the file is gone, so there is nothing to hash. It is
            // reported with an empty `current` rather than skipped, because "the migration this
            // installation ran no longer exists in the tree" is exactly the situation where an
            // operator needs to be told rather than left to discover it on the next build.
            None => drifts.push(Drift {
                version: version.clone(),
                name: name.clone(),
                recorded: recorded.clone(),
                current: String::new(),
            }),
            _ => {}
        }
    }
    drifts
}

/// The name after the `NNNN_` prefix, or the whole stem when there is no version prefix.
///
/// A file that does not match the naming convention is not accepted silently: the runner
/// validates the version separately (see [`validate_version`]), and this only has to produce
/// something readable for an error message. So the split is on the **first underscore**, but only
/// when what precedes it is digits — `no_version` is a malformed file whose whole stem is the
/// problem, not a migration called `version`.
///
/// The ledger's `name` column has to agree with this for every file in the tree, because
/// [`crate::lint`] looks a migration's findings up by `(version, name)` and a disagreement between
/// the two would hide a finding behind a lookup that cannot match.
#[must_use]
pub fn name_of(stem: &str) -> String {
    match stem.split_once('_') {
        Some((prefix, name))
            if !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit()) =>
        {
            name.to_owned()
        }
        _ => stem.to_owned(),
    }
}

/// Reject a version that is not `NNNN`.
///
/// The ledger's primary key carries the same check, so this is not belt and braces for
/// correctness — it is for the **message**. A `insert` that fails a check constraint says
/// `violates check constraint "version ~ ..."`, which does not name the file that caused it.
/// A runner that validates first can.
pub fn validate_version(version: &str) -> Result<()> {
    if version.len() < 4 || !version.chars().all(|c| c.is_ascii_digit()) {
        return Err(MigrationSafetyError::InvalidVersion {
            version: version.to_owned(),
            reason: "a migration file must be named NNNN_name.sql".to_owned(),
        });
    }
    Ok(())
}

/// Read the whole ledger, newest first.
///
/// A missing table is *not* swallowed: it means the installation has never run this migration,
/// which the caller reports as "no ledger yet" rather than as an empty ledger. The distinction
/// matters — an empty ledger and a ledger that could not be read render the same screen.
pub async fn list(pool: &PgPool) -> Result<Vec<LedgerRow>> {
    let rows = sqlx::query_as::<_, LedgerRow>(
        "select version, name, checksum, applied_at, duration_ms, statement_count, actor, \
                 source, has_down, down_verified_at, down_verified_by, waiver_reason \
         from schema_migrations order by version",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// One ledger row by version.
pub async fn read(pool: &PgPool, version: &str) -> Result<Option<LedgerRow>> {
    let row = sqlx::query_as::<_, LedgerRow>(
        "select version, name, checksum, applied_at, duration_ms, statement_count, actor, \
                 source, has_down, down_verified_at, down_verified_by, waiver_reason \
         from schema_migrations where version = $1",
    )
    .bind(version)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// The `(version, name, checksum)` triples the ledger holds, for [`detect_drift`].
pub async fn drift_input(pool: &PgPool) -> Result<Vec<(String, String, String)>> {
    let rows = sqlx::query_as::<_, (String, String, String)>(
        "select version, name, checksum from schema_migrations order by version",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Record a migration that SQLx has just committed.
///
/// Called **after** the apply, never before. That ordering is the whole reconciliation rule: a
/// ledger row means the database did it, so a crash between the two leaves a gap this reports
/// rather than a lie.
///
/// The insert is `on conflict do nothing` on the primary key and nothing else. A migration that
/// is already in the ledger has been applied once and must not gain a second row, but its
/// duration and actor must not be rewritten either — that would destroy the record of who
/// applied it the first time, which is the record the column exists for.
pub async fn record(pool: &PgPool, row: &NewLedgerRow) -> Result<()> {
    sqlx::query(
        "insert into schema_migrations \
             (version, name, checksum, duration_ms, statement_count, actor, source, has_down, \
              waiver_reason) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
         on conflict (version) do nothing",
    )
    .bind(&row.version)
    .bind(&row.name)
    .bind(&row.checksum)
    .bind(row.duration_ms)
    .bind(row.statement_count)
    .bind(&row.actor)
    .bind(&row.source)
    .bind(row.has_down)
    .bind(&row.waiver_reason)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark a migration's reversal as rehearsed.
///
/// The pair (`down_verified_at`, `down_verified_by`) is written together and the table's check
/// constraint refuses one without the other, so "verified by somebody at some point" can never
/// be a state a query can read back. This is the ONLY way either column gets a value — there is
/// no API that sets one alone, which is the mechanism behind the crate's
/// "a publisher's claim is not a verification" rule.
pub async fn mark_down_verified(pool: &PgPool, version: &str, by: &str) -> Result<()> {
    let affected = sqlx::query(
        "update schema_migrations set down_verified_at = now(), down_verified_by = $2 \
         where version = $1 and down_verified_at is null",
    )
    .bind(version)
    .bind(by)
    .execute(pool)
    .await?;

    if affected.rows_affected() == 0 {
        // Either the version is unknown or somebody already verified it. The two are not the
        // same answer, so the row is read back and the caller decides which message it prints.
        let existing = read(pool, version).await?;
        return match existing {
            None => Err(MigrationSafetyError::UnknownMigration {
                version: version.to_owned(),
            }),
            Some(row) if row.down_verified_at.is_some() => {
                Err(MigrationSafetyError::AlreadyVerified {
                    version: version.to_owned(),
                })
            }
            Some(_) => Ok(()),
        };
    }
    Ok(())
}

/// Every version SQLx has applied that this ledger has no row for.
///
/// This is the reconciliation gap in the direction that is safe to act on: the migration ran, the
/// ledger write did not (a crash between the two, or a binary from before this table existed).
/// The caller seeds these rows from the embedded files rather than inventing checksums.
pub async fn unrecorded(pool: &PgPool) -> Result<Vec<String>> {
    let rows = sqlx::query_as::<_, (String,)>(
        "select version from _sqlx_migrations where success \
           and version::text not in (select version from schema_migrations) order by version",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(version,)| version).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stable_file_hashes_to_a_stable_checksum() {
        // Pinned against `printf 'select 1;' | sha256sum`, not against the implementation: a test
        // that hashes the same input twice proves nothing about whether the encoding changed.
        // These values can be re-checked with a system tool, which is the point — an operator
        // verifying a ledger row by hand has to get the same answer this does.
        assert_eq!(
            checksum("select 1;"),
            "354b7196c9ba5fb4b21cf615bb6ec4cd5c07503c34229feef033fc081a8c03f4"
        );
        assert_eq!(
            checksum("create table if not exists schema_migrations ();"),
            "4e57c77de1349328654b34ad3408f067c709db6dabb71145925404f26c7ffd63"
        );
    }

    #[test]
    fn a_checksum_is_content_addressed_so_two_installations_agree() {
        // The property that makes a ledger comparison a string comparison: the same bytes produce
        // the same value on any machine, which is why an operator can check one with `sha256sum`
        // and why two installations' ledgers can be diffed against each other.
        assert_eq!(checksum("a"), checksum("a"));
        assert_ne!(
            checksum("a"),
            checksum("a\n"),
            "a trailing newline is a different file"
        );
        assert_eq!(checksum("").len(), 64, "an empty file still has a checksum");
    }

    #[test]
    fn a_deleted_applied_file_is_drift_not_a_pending_migration() {
        let ledger = vec![(
            "0199".to_owned(),
            "deployment_tooling".to_owned(),
            "aa".to_owned(),
        )];
        let drifts = detect_drift(&ledger, &[]);
        assert_eq!(
            drifts.len(),
            1,
            "an applied migration with no file is drift"
        );
        assert_eq!(drifts[0].version, "0199");
        assert!(drifts[0].current.is_empty(), "there is nothing to hash");
    }

    #[test]
    fn an_edited_applied_file_is_drift_and_the_message_names_it() {
        let ledger = vec![(
            "0199".to_owned(),
            "deployment_tooling".to_owned(),
            "aa".to_owned(),
        )];
        let files = vec![(
            "0199".to_owned(),
            "deployment_tooling".to_owned(),
            "bb".to_owned(),
        )];
        let drifts = detect_drift(&ledger, &files);
        assert_eq!(drifts.len(), 1);
        let message = drifts[0].message();
        assert!(
            message.contains("0199_deployment_tooling"),
            "names the file: {message}"
        );
        assert!(
            message.contains("a new migration"),
            "names the fix: {message}"
        );
    }

    #[test]
    fn an_unapplied_file_is_pending_not_drift() {
        // The asymmetry that matters: a new migration is the NORMAL state and must never be
        // reported as drift, or every author's first push after a fresh clone fails the runner.
        let drifts = detect_drift(
            &[],
            &[(
                "0207".to_owned(),
                "migration_safety".to_owned(),
                "cc".to_owned(),
            )],
        );
        assert!(
            drifts.is_empty(),
            "a file with no ledger row is pending: {drifts:?}"
        );
    }

    #[test]
    fn a_version_must_be_four_digits_and_is_refused_with_the_naming_rule() {
        assert!(validate_version("0207").is_ok());
        assert!(
            validate_version("207").is_err(),
            "a three-digit version sorts wrong in every listing"
        );
        let err = validate_version("release_1").unwrap_err().to_string();
        assert!(
            err.contains("NNNN_name.sql"),
            "the message states the convention: {err}"
        );
    }

    #[test]
    fn the_name_is_everything_after_the_first_underscore() {
        assert_eq!(name_of("0199_deployment_tooling"), "deployment_tooling");
        assert_eq!(
            name_of("0199_a_b_c"),
            "a_b_c",
            "only the FIRST underscore separates"
        );
        assert_eq!(
            name_of("no_version"),
            "no_version",
            "a file with no prefix is reported whole"
        );
    }
}
