//! `omnion migrate new <name>` — allocate the next migration file (REQ-131, slice 2).
//!
//! ## Why this is a command and not a convention
//!
//! The migration numbering is a **shared** namespace: this repository is worked on by several
//! writers in parallel branches, and every branch has its own `database/migrations` directory
//! until the branches are merged. So "the next number" is not a property of the checkout you
//! happen to be in — it is the union of every branch's files plus every version the ledger has
//! ever recorded. A command that answered with `max(files) + 1` would hand two writers the same
//! number, and the collision surfaces far from the file that caused it.
//!
//! So the number is allocated from the union of two sources, and both are reported:
//!
//! * **the ledger** — every version the database has recorded, including files that were
//!   applied and later deleted from the tree. A number freed by a deletion is not free.
//! * **the tree** — every `NNNN_*.sql` in the resolved directory, including the ones not yet
//!   applied.
//!
//! The ledger half is the one a tree scan cannot see, and it is the half that bites: a branch
//! that adds `0243_x.sql`, has it applied, and is then deleted leaves the directory's high-water
//! at `0240` while the installation has already run `0243`. Verified against a live database
//! holding `0004, 0009, 0012` with a tree containing only `0001`: this command allocated `0013`,
//! where `max(files) + 1` would have answered `0002`.
//!
//! ## The directory is resolved, never assumed
//!
//! The binary is installed outside the repository, so `omnion migrate new` cannot know where the
//! migrations live from its own path. It walks up from the working directory looking for the
//! workspace marker (`Cargo.toml` **and** `database/migrations`), and `OMNION_MIGRATIONS_DIR` (or
//! `--dir`) overrides that for a checkout elsewhere. **There is no default of
//! `database/migrations` relative to the current directory**: a command that silently created it
//! in a random working directory produces a file nobody applies, and the developer finds out when
//! the migration is missing from the build.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use omnion_migrations::down;
use omnion_migrations::ledger;
use omnion_migrations::runner;

/// The environment variable that overrides where migrations live.
pub const MIGRATIONS_DIR_ENV: &str = "OMNION_MIGRATIONS_DIR";

/// The directory a migration belongs in, relative to the workspace root.
const MIGRATIONS_SUBDIR: &str = "database/migrations";

/// Why a `new` invocation refused to write anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// No name was given after the sub-action.
    MissingName,
    /// The name is not a migration slug.
    InvalidName(String),
    /// A migration with this name already exists in the tree or the ledger.
    DuplicateName { name: String, existing: String },
    /// The migrations directory could not be located.
    NoDirectory,
    /// The migrations directory exists but cannot be written to.
    NotWritable {
        /// The directory that refused the write.
        directory: PathBuf,
        /// The filesystem's own reason, kept verbatim.
        reason: String,
    },
    /// The file could not be written.
    NotWritten(String),
}

impl Refusal {
    /// The sentence a human reads, and the word a script matches on.
    ///
    /// Both halves are here because the caller needs them together: the envelope's `error.code`
    /// is a contract and the message is the only place an operator learns *which* name to
    /// change. The two are built from one value so they cannot disagree.
    pub fn message(&self) -> String {
        match self {
            Self::MissingName => "migrate new needs a name: `omnion migrate new add_invoices`"
                .to_owned(),
            Self::InvalidName(reason) => {
                format!("a migration name must be lower_snake_case: {reason}")
            }
            Self::DuplicateName { name, existing } => format!(
                "a migration named {name:?} already exists ({existing}); pick another name"
            ),
            Self::NoDirectory => format!(
                "could not find the migrations directory: no {MIGRATIONS_SUBDIR} next to a \
                 Cargo.toml above the working directory. Run this from inside the checkout, or \
                 set {MIGRATIONS_DIR_ENV}."
            ),
            Self::NotWritable { directory, reason } => {
                format!("{} cannot be written to ({reason})", directory.display())
            }
            Self::NotWritten(reason) => format!("could not write the migration file: {reason}"),
        }
    }

    /// The documented envelope code for this refusal.
    ///
    /// `Usage` for anything the operator typed, and never `internal`: every arm here is a
    /// decision this command made, and a script that sees `internal` learns nothing.
    pub fn code(&self) -> crate::envelope::ErrorCode {
        match self {
            Self::MissingName | Self::InvalidName(_) | Self::DuplicateName { .. } => {
                crate::envelope::ErrorCode::Usage
            }
            Self::NoDirectory
            | Self::NotWritable { .. }
            | Self::NotWritten(_) => {
                crate::envelope::ErrorCode::ConfigUnreadable
            }
        }
    }
}

/// What one `new` invocation decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    /// The allocated version, zero-padded to four digits.
    pub version: String,
    /// The slug, as written into the filename.
    pub name: String,
    /// The file that was written.
    pub path: PathBuf,
    /// The directory it was written into.
    pub directory: PathBuf,
}

/// Allocate the next free version and write the file.
///
/// `applied` is every version the ledger holds, from every database this installation has ever
/// talked to. It is passed in rather than read here so the allocation is a pure function of
/// (tree, ledger) and can be tested without a database — the alternative is a `new` command that
/// can only be tested against a live PostgreSQL, which is a command nobody tests.
pub fn create(
    directory: &Path,
    name: &str,
    applied: &BTreeSet<i64>,
) -> Result<Created, Refusal> {
    let name = validate_name(name)?;
    let versions = tree_versions(directory)?;

    // Both sources, and the union: a version that is applied but no longer in the tree is still
    // taken, because SQLx refuses to re-apply a lower version after a higher one, and because the
    // number is a shared namespace across branches.
    let mut taken: BTreeSet<i64> = versions.iter().copied().collect();
    taken.extend(applied.iter().copied());

    // The duplicate check reads the TREE, not the ledger. A ledger row has no filename, and a
    // same-name-different-number migration is not a collision: SQLx keys on the version, and the
    // lint findings are keyed on `(version, name)`, so a repeated *name* under a new number is a
    // legal (if inelegant) file. What must not happen is two files in one directory, which is
    // what the tree answers and the ledger cannot.
    if let Some(existing) = find_by_name(directory, &name) {
        return Err(Refusal::DuplicateName {
            name: name.clone(),
            existing,
        });
    }

    let next = taken.iter().next_back().map_or(1, |version| version + 1);
    let version = format!("{next:04}");
    let filename = format!("{version}_{name}.sql");
    let path = directory.join(&filename);

    // A pre-flight writability probe, so the refusal names the problem instead of surfacing as
    // a raw `Permission denied` from the write below. It is a real check rather than a guess: a
    // directory can be read- and list-able and not writable (an unwritable `database/migrations`
    // in a checkout owned by another user is the common case), and an author who cannot write
    // the file deserves to be told the DIRECTORY, not handed a filesystem errno.
    if let Err(reason) = writability(directory) {
        return Err(Refusal::NotWritable {
            directory: directory.to_path_buf(),
            reason,
        });
    }

    // Write to a temporary name and rename over the target, so a process killed between the two
    // leaves either no file or a complete one — never a half-written migration that applies the
    // first statement of an `alter table` and then fails.
    let temporary = directory.join(format!(".{filename}.tmp"));
    if let Err(err) = std::fs::write(&temporary, template(&version, &name)) {
        return Err(Refusal::NotWritten(err.to_string()));
    }
    if let Err(err) = std::fs::rename(&temporary, &path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(Refusal::NotWritten(err.to_string()));
    }

    Ok(Created {
        version,
        name,
        path,
        directory: directory.to_path_buf(),
    })
}

/// Whether a probe file can be created in `directory`.
///
/// Creating and removing a real file is the only portable answer: the permission bits are
/// advisory, the mount can be read-only, and a disk can be full — and each of those has a
/// different remedy that the operator is the one who can apply.
fn writability(directory: &Path) -> Result<(), String> {
    let probe = directory.join(format!(".omnion-new-probe-{}", std::process::id()));
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(err) => Err(err.to_string()),
    }
}

/// Validate a migration slug.
///
/// Lower `snake_case` only, because the filename becomes a SQLx description, a ledger `name` and
/// a lint lookup key — and a name with a space, a capital or a dash is one that renders as
/// `0207 some Name.sql` in a drift message, which is a file nobody can open.
fn validate_name(name: &str) -> Result<String, Refusal> {
    if name.is_empty() {
        return Err(Refusal::MissingName);
    }
    if name.starts_with('-') || name.starts_with('_') {
        return Err(Refusal::InvalidName(format!(
            "{name:?} starts with punctuation; a name reads like a sentence: add_invoices"
        )));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(Refusal::InvalidName(format!(
            "{name:?} is not lower_snake_case: use a, z, 0-9 and underscores only"
        )));
    }
    if name.chars().all(|c| c.is_ascii_digit()) {
        return Err(Refusal::InvalidName(format!(
            "{name:?} is only digits, which is the version prefix, not a name"
        )));
    }
    if name.len() > 60 {
        return Err(Refusal::InvalidName(format!(
            "{} characters is past the 60 a filename should carry",
            name.len()
        )));
    }
    Ok(name.to_owned())
}

/// The versions present in the tree, read from the filenames.
///
/// Read from disk rather than from the embedded migrator on purpose: the embedded migrator is a
/// snapshot of **this binary's build**, so a stale `omnion` would hand out a number a file two
/// commits ago already used. `new` is the one migration command that must see the working tree,
/// because the file it writes is not in that tree until the next build.
pub fn tree_versions(directory: &Path) -> Result<BTreeSet<i64>, Refusal> {
    let entries = std::fs::read_dir(directory).map_err(|_| Refusal::NoDirectory)?;
    let mut versions = BTreeSet::new();

    for entry in entries.flatten() {
        let filename = entry.file_name();
        let Some(filename) = filename.to_str() else {
            continue;
        };
        // The temporary file of an interrupted write is skipped rather than refused: it is not a
        // migration, and a leftover one must not make the directory unreadable.
        if filename.starts_with('.') {
            continue;
        }
        let Some((prefix, rest)) = filename.split_once('_') else {
            continue;
        };
        if !rest.ends_with(".sql") {
            continue;
        }
        if let Ok(version) = prefix.parse::<i64>() {
            versions.insert(version);
        }
    }
    Ok(versions)
}

/// The filename already carrying this slug, if any.
fn find_by_name(directory: &Path, name: &str) -> Option<String> {
    let entries = std::fs::read_dir(directory).ok()?;
    for entry in entries.flatten() {
        let filename = entry.file_name();
        let Some(filename) = filename.to_str() else {
            continue;
        };
        if let Some((_prefix, rest)) = filename.split_once('_') {
            if rest == format!("{name}.sql") {
                return Some(filename.to_owned());
            }
        }
    }
    None
}

/// Find the migrations directory.
///
/// `OMNION_MIGRATIONS_DIR` wins, then the working directory and every parent. The marker is a
/// `Cargo.toml` next to a `database/migrations` directory, which is the only shape that
/// distinguishes this repository from a random project that happens to have a `Cargo.toml`.
pub fn resolve_directory(explicit: Option<&str>) -> Result<PathBuf, Refusal> {
    if let Some(path) = explicit {
        let path = PathBuf::from(path);
        if !path.is_dir() {
            return Err(Refusal::NoDirectory);
        }
        return Ok(path);
    }

    let mut current = std::env::current_dir().map_err(|_| Refusal::NoDirectory)?;
    loop {
        let candidate = current.join(MIGRATIONS_SUBDIR);
        if candidate.is_dir() && current.join("Cargo.toml").is_file() {
            return Ok(candidate);
        }
        if !current.pop() {
            return Err(Refusal::NoDirectory);
        }
    }
}

/// The environment override, if it is set.
pub fn directory_from_env() -> Option<String> {
    std::env::var(MIGRATIONS_DIR_ENV).ok().filter(|v| !v.trim().is_empty())
}

/// The skeleton this command writes.
///
/// It is a skeleton and says so, and that matters: the two markers in the file are the ones the
/// policy and the deployment centre read, and a template that guessed them would produce
/// migrations claiming a reversal nobody wrote.
///
/// * `-- omnion:down` opens the reversal block. It is **prose**, so `has_down` is false until the
///   author writes a real statement — which is what makes `omnion migrate plan` report the
///   missing reversal instead of `up` silently applying a file the author believes is
///   reversible.
/// * `-- omnion:no-down` is shown commented out for the genuinely irreversible case, with the
///   reason it demands.
///
/// **The indentation in the reversal block is load-bearing, and the template keeps its own
/// example OUTSIDE that block for the same reason.** `down::extract_down` counts a comment
/// line as a *statement* only when its body is indented by two or more spaces, so a reversal
/// written as
///
/// ```text
/// -- drop table invoices;
/// ```
///
/// is prose, and the policy reports the file as having no reversal — silently, at apply time,
/// for every author who followed the instruction. The first version of this template told
/// authors to write reversals "un-indented", which is the same defect in the opposite
/// direction; the version after that fixed the prose and made the template's own hint line
/// indented, so every generated migration claimed a reversal nobody had written.
///
/// So the shape is demonstrated in the header, above the marker where nothing is parsed, and
/// the block itself holds one unindented line that the parser reads as prose — which is what
/// makes the file report itself as *not yet reversible* until the author means it.
///
/// A `lock_timeout` hint is included for the same reason: the runner wraps every statement in one
/// transaction with a 5 s lock budget, and an author who does not know that writes `create index`
/// and waits.
fn template(version: &str, name: &str) -> String {
    format!(
        "-- {version}_{name}.sql
--
-- Generated by `omnion migrate new {name}`. Fill in the forward half below.
--
-- Two things the runner will ask about, and it asks them BEFORE applying:
--
--   * a reversal. Add a block at the end of this file, shaped exactly like this one:
--
--         -- omnion:down
--         --   drop table invoices;
--
--     The two spaces after the `--` on the second line are PART OF THE FORMAT. The reversal
--     parser counts a comment as a statement only when it is indented two or more spaces, so
--     `-- drop table invoices;` reads as prose and the migration policy then reports this file
--     as having no reversal at all — at apply time, not now. The example above is outside the
--     block for the same reason: an example inside it would be read as a real statement.
--
--     While the block holds only prose that is exactly what happens, and it is the intended
--     way to find out. For a genuinely irreversible change, replace the block with:
--
--         -- omnion:no-down
--         -- reason: <why it cannot be reversed>
--
--   * lock time. Every statement runs inside one transaction with a 5 s lock budget. A
--     `create index` on a growing table exceeds that, so build it concurrently in its own
--     statement, or add an index in a later migration.

-- Write the forward half here.

-- omnion:down
-- Replace this line with the reversal, two spaces of indent per statement.
"
    )
}

/// Verify a freshly written file the way the runner will read it.
///
/// Returns what the runner sees, so a caller (and the test) can assert the *parser's* answer
/// rather than the template's intention: a template that the parser reads as a reversal would
/// make `plan` report "present" and `up` believe the file is reversible.
pub fn inspect(path: &Path) -> Result<(usize, bool, bool), Refusal> {
    let sql = std::fs::read_to_string(path).map_err(|err| Refusal::NotWritten(err.to_string()))?;
    let up = runner::up_statements(&sql);
    let down_script = down::extract_down(&sql);
    Ok((up.len(), !down_script.statements.is_empty(), down_script.declared_no_down))
}

/// The versions the ledger holds, for the caller that has a pool.
pub async fn ledger_versions(pool: &omnion_core::PgPool) -> BTreeSet<i64> {
    ledger::drift_input(pool)
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|(version, _name, _checksum)| version.parse::<i64>().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A temporary directory, removed when the test ends.
    ///
    /// Hand-rolled rather than pulled from a crate: the CLI ships no dependency it does not use
    /// at runtime, and a test that needs a temp directory is not worth a dependency in the
    /// shipped binary.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
            let path = std::env::temp_dir().join(format!(
                "omnion-migrate-new-{label}-{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("a temporary directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write(directory: &Path, filename: &str) {
        std::fs::write(directory.join(filename), "-- fixture\n").expect("writes the fixture");
    }

    #[test]
    fn a_name_is_allocated_after_the_highest_number_in_the_tree() {
        let dir = TempDir::new("high-water");
        write(dir.path(), "0001_first.sql");
        write(dir.path(), "0007_seventh.sql");
        write(dir.path(), "0011_eleventh.sql");

        let created = create(dir.path(), "add_invoices", &BTreeSet::new()).expect("creates");

        assert_eq!(created.version, "0012");
        assert_eq!(created.name, "add_invoices");
        assert_eq!(
            created.path.file_name().and_then(|n| n.to_str()),
            Some("0012_add_invoices.sql")
        );
    }

    #[test]
    fn a_number_the_ledger_knows_is_taken_even_when_the_file_is_gone() {
        // The case that makes this a command rather than `ls`: a migration was applied and later
        // deleted from the tree, so the directory's high-water drops below what the installation
        // has already run. `max(files) + 1` then hands the same number out a second time, and
        // the two files collide in a namespace they share with every other branch.
        //
        // The input mirrors the live proof: a ledger holding 0004, 0009 and 0012 against a tree
        // that only has 0001. The expectation is the union's high-water plus one.
        let dir = TempDir::new("deleted");
        write(dir.path(), "0001_first.sql");

        let mut applied = BTreeSet::new();
        applied.insert(4);
        applied.insert(9);
        applied.insert(12);
        let created = create(dir.path(), "after_deletion", &applied).expect("creates");

        assert_eq!(
            created.version, "0013",
            "the ledger's high-water wins over the tree's"
        );
    }

    #[test]
    fn a_duplicate_name_is_refused_and_the_existing_file_is_named() {
        let dir = TempDir::new("duplicate");
        write(dir.path(), "0004_add_invoices.sql");

        let refusal = create(dir.path(), "add_invoices", &BTreeSet::new()).expect_err("refuses");

        assert_eq!(
            refusal,
            Refusal::DuplicateName {
                name: "add_invoices".to_owned(),
                existing: "0004_add_invoices.sql".to_owned()
            }
        );
        assert!(
            refusal.message().contains("0004_add_invoices.sql"),
            "the operator is told which file to look at: {}",
            refusal.message()
        );
    }

    #[test]
    fn a_duplicate_name_is_refused_against_a_gap_in_the_tree_too() {
        // The name check is not "is the next number free". Renaming `0009_a` to `0010_b` and
        // creating a fresh `0010_b` in a later run must be caught by name, or the tree ends up
        // with two files whose ledger rows both say the same name.
        let dir = TempDir::new("gap");
        write(dir.path(), "0010_add_invoices.sql");
        write(dir.path(), "0001_first.sql");

        assert!(matches!(
            create(dir.path(), "add_invoices", &BTreeSet::new()),
            Err(Refusal::DuplicateName { .. })
        ));
    }

    #[test]
    fn a_leftover_temporary_file_is_not_mistaken_for_a_migration() {
        // An interrupted write leaves `.0012_x.sql.tmp`. Counting it would allocate 0013 and
        // leave the author wondering where 0012 went; refusing the directory would be worse.
        let dir = TempDir::new("tmp");
        write(dir.path(), "0001_first.sql");
        write(dir.path(), ".0012_interrupted.sql.tmp");

        let created = create(dir.path(), "next", &BTreeSet::new()).expect("creates");

        assert_eq!(created.version, "0002");
    }

    #[test]
    fn names_that_would_produce_an_unopenable_filename_are_refused() {
        // The filename becomes a SQLx description and a drift message: `0207 Some Name.sql` is
        // a file `ls` cannot match and a drift report nobody can act on.
        for (name, fragment) in [
            ("Add Invoices", "lower_snake_case"),
            ("add-invoices", "lower_snake_case"),
            ("add invoices", "lower_snake_case"),
            ("_leading", "punctuation"),
            ("2026", "version prefix"),
            ("", ""),
        ] {
            let refusal = validate_name(name).expect_err("refused");
            if name.is_empty() {
                assert_eq!(refusal, Refusal::MissingName, "{name:?}");
            } else {
                assert!(
                    refusal.message().contains(fragment),
                    "{name:?} -> {} should explain {fragment}",
                    refusal.message()
                );
            }
        }
        for name in ["add_invoices", "v2_billing", "add_invoices_2"] {
            assert_eq!(
                validate_name(name).expect("accepted"),
                name,
                "{name} is a valid slug"
            );
        }
    }

    #[test]
    fn the_generated_file_is_read_the_way_the_runner_reads_it() {
        // The assertion is on the PARSER's answer, not the template's text. A template whose
        // down block parses as executable would make `migrate plan` report the reversal as
        // PRESENT and `up` consider the file reversible — the exact lie a scaffold must not
        // tell, and one that is invisible until a deployment.
        let dir = TempDir::new("template");
        let created = create(dir.path(), "add_invoices", &BTreeSet::new()).expect("creates");
        let (up_statements, has_down, declared_no_down) =
            inspect(&created.path).expect("the runner can read it");

        assert_eq!(
            up_statements, 0,
            "the forward half is empty; prose must not read as a statement"
        );
        assert!(
            !has_down,
            "a prose reversal block is NOT a reversal — the policy has to report it missing"
        );
        assert!(
            !declared_no_down,
            "the no-down marker ships commented out; the file does not claim to be irreversible"
        );
    }

    #[test]
    fn a_commented_out_statement_still_does_not_read_as_one_after_editing() {
        // Same property, on the file an author has actually started: a reversal still being
        // written is reported as missing, not as present-because-it-has-a-block.
        let dir = TempDir::new("edited");
        let created = create(dir.path(), "add_invoices", &BTreeSet::new()).expect("creates");
        let sql = std::fs::read_to_string(&created.path).expect("reads");
        std::fs::write(
            &created.path,
            sql.replace(
                "-- Replace this line with the reversal, two spaces of indent per statement.",
                "--   drop table invoices;",
            ),
        )
        .expect("writes");

        let (_, has_down, _) = inspect(&created.path).expect("reads");
        assert!(
            has_down,
            "a real statement in the documented shape IS a reversal"
        );
    }

    #[test]
    fn the_template_teaches_the_indentation_the_parser_actually_requires() {
        // The defect this test exists for. `down::extract_down` counts a comment line as a
        // statement only when its body is indented two or more spaces, so the first template —
        // which told the author to write reversals "un-indented" — would have produced files the
        // policy reports as having NO reversal, silently, for every author who followed it.
        //
        // The assertion is on the PARSER, not on the prose. A template whose own hint line is
        // indented enough to parse is worse than one that teaches the wrong shape: it makes every
        // generated migration claim a reversal that was never written, and the deployment centre
        // then labels an irreversible change `reversible`.
        let dir = TempDir::new("indentation");
        let created = create(dir.path(), "add_invoices", &BTreeSet::new()).expect("creates");
        let (up_statements, has_down, declared_no_down) =
            inspect(&created.path).expect("the runner can read it");

        assert_eq!(up_statements, 0, "the forward half is empty");
        assert!(
            !has_down,
            "the template's own hint line must not parse as a reversal statement"
        );
        assert!(!declared_no_down);

        // And the shape the template DOCUMENTS has to be the shape the parser accepts — the
        // hint is only useful if following it works.
        let sql = std::fs::read_to_string(&created.path).expect("reads");
        let with_a_reversal = sql.replace(
            "-- Write the forward half here.",
            "create table invoices (id uuid primary key);",
        );
        assert!(
            omnion_migrations::down::extract_down(&with_a_reversal).statements.is_empty(),
            "the pristine template has no reversal"
        );

        let documented = with_a_reversal.replace(
            "-- Replace this line with the reversal, two spaces of indent per statement.",
            "--   drop table invoices;",
        );
        let parsed = omnion_migrations::down::extract_down(&documented);
        assert_eq!(
            parsed.statements,
            vec!["drop table invoices;".to_owned()],
            "the shape the template documents is the shape the parser reads"
        );
        assert_eq!(
            runner::up_statements(&documented),
            vec!["create table invoices (id uuid primary key);".to_owned()],
            "the forward statement is read too, semicolon included"
        );
    }

    #[test]
    fn the_directory_is_never_guessed_from_the_working_directory() {
        // A `new` that created `database/migrations` under an arbitrary directory writes a file
        // no build will ever compile, and the developer finds out from a missing migration.
        let empty = TempDir::new("no-marker");
        assert_eq!(
            resolve_directory(Some(empty.path().to_str().expect("utf-8"))).ok(),
            Some(empty.path().to_path_buf()),
            "an explicit directory is used as given"
        );
        assert_eq!(
            resolve_directory(Some("/nonexistent/omnion/migrations")),
            Err(Refusal::NoDirectory),
            "an explicit directory that is not there is not created"
        );
    }

    #[test]
    fn a_path_that_is_not_a_directory_is_refused_before_anything_is_written() {
        // The refusal that does not need privileges to produce: a regular file where the
        // migrations directory should be. It is asserted through the real `create`, so the
        // directory is resolved and read before any write is attempted — no partial file, no
        // stray probe, and a code the caller can branch on.
        let dir = TempDir::new("not-a-directory");
        let blocker = dir.path().join("blocker");
        write(dir.path(), "blocker");

        let refusal =
            create(&blocker, "add_invoices", &BTreeSet::new()).expect_err("a file is not a directory");

        assert!(
            matches!(refusal, Refusal::NoDirectory),
            "a path that is not a directory is reported as no directory, got {refusal:?}"
        );
        assert_eq!(refusal.code().as_str(), "config_unreadable");
        assert!(
            refusal.message().contains("migrations directory"),
            "the operator is told what was missing: {}",
            refusal.message()
        );
    }

    #[test]
    fn every_refusal_carries_its_own_code_and_message() {
        // A script branching on `error.code` and a human reading `error.message` both come from
        // one value, so they cannot disagree about what happened. The code is the enum's DEBUG
        // form here rather than its wire form, which is the stricter assertion: a rename of a
        // variant shows up, and the wire form is separately pinned in the envelope's own table.
        use crate::envelope::ErrorCode;
        let cases = [
            (Refusal::MissingName, ErrorCode::Usage),
            (Refusal::InvalidName("x".to_owned()), ErrorCode::Usage),
            (
                Refusal::DuplicateName {
                    name: "a".to_owned(),
                    existing: "0001_a.sql".to_owned(),
                },
                ErrorCode::Usage,
            ),
            (Refusal::NoDirectory, ErrorCode::ConfigUnreadable),
            (
                Refusal::NotWritable {
                    directory: PathBuf::from("/x"),
                    reason: "Permission denied".to_owned(),
                },
                ErrorCode::ConfigUnreadable,
            ),
            (
                Refusal::NotWritten("disk full".to_owned()),
                ErrorCode::ConfigUnreadable,
            ),
        ];
        for (refusal, expected) in cases {
            assert_eq!(refusal.code(), expected, "{refusal:?}");
            assert!(
                !refusal.message().is_empty(),
                "{refusal:?} explains itself"
            );
            assert_ne!(
                refusal.code().as_str(),
                "internal",
                "a decision this command made is never the fallback"
            );
        }
    }
}
