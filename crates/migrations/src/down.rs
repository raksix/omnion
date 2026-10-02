//! Reading a migration's reversal out of the migration file itself
//! (docs/requests/REQ-129, slice 1).
//!
//! ## Why the reversal is not a second file
//!
//! This repository's runner is a single forward-only `sqlx::migrate!` bundle embedded at compile
//! time, so a `NNNN_name.down.sql` sibling would not be read by anything that exists today. It
//! would be a file nobody runs, which is worse than no file: the gate below would have to decide
//! whether to trust it, and the answer would be "yes" for a file no code path has ever opened.
//!
//! So the reversal lives inside the file as a marked comment block, next to the statements it
//! reverses, and **this module is the only thing that reads it**. That is what makes "has a down
//! script" a question with an answer rather than a grep somebody ran once.
//!
//! ## The block is recognised by a heading AND a marker, and one of them is the convention
//!
//! My first version keyed the block on a line that cannot occur in prose -- the `omnion:down`
//! marker -- and **zero of this repository's 58 migrations used it**, including the two files
//! that document the convention most carefully. The convention that actually exists is a headed
//! block:
//!
//! ```text
//! -- ---------------------------------------------------------------------------
//! -- Down script (docs/05-VERSIONING.md)
//! -- ---------------------------------------------------------------------------
//! ```
//!
//! So the marker is still supported -- a future migration may prefer it and a file carrying it
//! must keep working -- but it is the SECOND way in, not the only one. A parser that reads the
//! marker alone is not stricter, it is broken: it reports `has_down = false` for every file in
//! the tree, and the gate consuming that answer refuses to apply any of them.
//!
//! ### Why the heading is matched tightly
//!
//! "Down script" appears in prose all over this repository: `0207_migration_safety.sql` says
//! "no down script was found in the file" inside a `create table`, and `0035` explains why the
//! reversal is a comment. A substring search for the phrase opens a block in the middle of a
//! column comment and then treats every indented comment after it as the reversal.
//!
//! So a heading must be the WHOLE comment line -- optionally `##`-prefixed and optionally followed
//! by a parenthesised reference. That excludes "no down script was found" and includes
//! `-- ## Down script`, which two files use. It is still a phrase match and that is stated rather
//! than hidden: a heading only ever OPENS a block, and the block ends at the first statement that
//! is not part of one.
//!
//! The original argument for the marker is preserved below, because the conclusion it reached is
//! the one that matters:

//!
//! ```text
//! -- omnion:down
//! ```
//!
//! ## Inside the block, indentation is the statement separator
//!
//! A comment line whose content is indented by **two or more spaces** is a *statement*; anything
//! else is commentary on the reversal. That is the same convention `0162_reliability.sql` and
//! `0199_deployment_tooling.sql` already use for their reversals, and it is the reason those two
//! files need no edits to pass this gate.
//!
//! ## A block with no indented statements is prose, and prose is a failure
//!
//! Not a pass. This is the rule the request cares about most: "a down script is written down
//! somewhere in this file" is not the same claim as "this migration can be reversed", and only
//! the second one lets the deployment centre say `reversible` instead of `unknown`. A migration
//! whose block is all commentary therefore reports `has_down = false`, which routes it through the
//! policy check and the upgrade helper — both of which treat it as "take the backup".

/// The explicit marker that opens (and closes) a reversal block.
///
/// One of the two ways in; see the module doc for why the heading is the convention and this is
/// the opt-in. A file carrying the marker and a file carrying the heading both parse, which is
/// what lets this crate ship without touching 58 files that predate it.
pub const DOWN_MARKER: &str = "omnion:down";

/// The marker a maintainer puts in a file that has no reversal and says so on purpose.
///
/// This is what `omnion_deployment::manifest::destructiveness` already reads for the upgrade
/// plan's verdict, and it is kept here so the two sides of the same fact cannot drift: the plan
/// asks "did the author declare this irreversible" and this crate asks "is there a reversal", and
/// the answer to the first is only meaningful next to the second.
pub const NO_DOWN_MARKER: &str = "omnion:no-down";

/// The statements of one migration's reversal, and whether it declared itself irreversible.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DownScript {
    /// Executable statements, in file order, with the leading `--` and the indent removed.
    ///
    /// Empty means **no reversal was found**, which is not the same as "the reversal is empty" —
    /// an empty reversal would be a no-op that reports success, and nothing here can produce one.
    pub statements: Vec<String>,
    /// The file declared itself irreversible with `-- omnion:no-down`.
    pub declared_no_down: bool,
    /// Line number (1-based) the block opens on, for the plan preview's error messages. `None`
    /// when there is no block at all.
    pub block_start: Option<usize>,
}

impl DownScript {
    /// `true` when there is at least one statement to run.
    #[must_use]
    pub fn has_statements(&self) -> bool {
        !self.statements.is_empty()
    }

    /// The reversal as one SQL string, ready for `sqlx::raw_sql`.
    ///
    /// Returns `None` rather than an empty string when there is nothing to run, so the caller
    /// cannot accidentally execute a no-op and record a successful reversal. That distinction is
    /// the entire reason this type exists instead of a `String`.
    #[must_use]
    pub fn sql(&self) -> Option<String> {
        if self.statements.is_empty() {
            return None;
        }
        Some(self.statements.join("\n"))
    }
}

/// Extract the reversal from a migration file's content.
///
/// Pure and total: every input produces a value, there is no error case, and there is no input for
/// which it can panic or loop. A parse that can fail has to be told what a failure means, and
/// "this file has no reversal" already means something precise.
#[must_use]
pub fn extract_down(content: &str) -> DownScript {
    // The marker wins over the heading, and the order is a decision rather than an accident.
    //
    // A file may legitimately carry BOTH: `0199_deployment_tooling.sql` explains the convention in
    // prose with a `## Down script` heading near the top and then writes the real reversal under an
    // explicit marker further down. A single pass would open at the *first* thing it recognised —
    // the explanatory heading — and either close on the marker (yielding nothing) or read the
    // marker's own line as the block's first statement. Two passes with a priority make the answer
    // depend on which form is more explicit rather than on which appears first, and the
    // marker-vs-heading question then has exactly one answer per file.
    //
    // The fallback condition is "produced no statements", not "no marker present": a marker block
    // that is all commentary is prose, and a file with a prose marker AND a headed reversal should
    // still report the headed one.
    let marked = scan(content, |body| body.trim() == DOWN_MARKER);
    if marked.has_statements() {
        return DownScript {
            declared_no_down: declares_no_down(content),
            ..marked
        };
    }
    let headed = scan(content, is_down_heading);
    DownScript {
        declared_no_down: declares_no_down(content),
        ..headed
    }
}

/// One pass for one way in. `opens` decides whether a comment line starts a block.
fn scan(content: &str, opens: fn(&str) -> bool) -> DownScript {
    let mut script = DownScript::default();

    let mut inside = false;
    for (index, line) in content.lines().enumerate() {
        if let Some(body) = comment_body(line) {
            if opens(body) {
                if inside {
                    break; // a second opener ends the block; nothing after it is a statement
                }
                inside = true;
                script.block_start = Some(index + 1);
                continue;
            }
        }
        if !inside {
            continue;
        }
        // Inside the block: a comment whose content is indented by two or more spaces is a
        // statement. Everything else is commentary and is dropped, which is what lets the
        // reversal explain itself without the statements having to be a bare list.
        if let Some(body) = comment_body(line) {
            let indent = body.len() - body.trim_start().len();
            let trimmed = body.trim();
            if indent >= 2 && !trimmed.is_empty() {
                script.statements.push(trimmed.to_owned());
            }
        }
    }
    script
}

/// `true` when a comment line is a down-script heading rather than prose mentioning one.
///
/// Accepted shapes, and nothing else:
///   * `-- Down script`
///   * `-- Down script (docs/05-VERSIONING.md)`
///   * `-- ## Down script`
///
/// Rejected, and each for a reason that is a real file in this tree:
///   * `-- the down script exists as a comment because …` — a sentence, not a heading
///     (`0035_observability_logs.sql`).
///   * `-- `false` means "no down script was found in the file"` — a column comment inside a
///     `create table` (`0207_migration_safety.sql`).
///   * `-- Reverse order, children before parents` — the line AFTER the heading, which mentions
///     neither the phrase nor a marker and is therefore commentary.
///
/// The match is on the trimmed body with a leading `##` stripped, so the rule above is exact:
/// nothing matches unless the heading word is the entire content.
#[must_use]
pub fn is_down_heading(body: &str) -> bool {
    let body = body.trim();
    let body = body.strip_prefix("##").map(str::trim_start).unwrap_or(body);
    let Some(rest) = body.strip_prefix(DOWN_HEADING).map(str::trim) else {
        return false;
    };
    // Whatever follows must be empty or a parenthesised reference — `(docs/05-VERSIONING.md)`.
    // A sentence does not match, and neither does a second word.
    rest.is_empty() || (rest.starts_with('(') && rest.ends_with(')'))
}

/// The heading phrase, without the comment marker.
pub const DOWN_HEADING: &str = "Down script";

/// The text after `--`, or `None` when the line is not a SQL comment.
///
/// Handles `--`, `-- ` and `--\t` uniformly, and returns the comment body **with its leading
/// whitespace intact** — the indent is the statement marker, so trimming it here would destroy
/// the one piece of information [`extract_down`] needs.
fn comment_body(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix("--")?;
    // A line of only dashes is a separator (`-----`), not a comment with content. It is harmless
    // either way — its body is dashes, which is neither the marker nor an indented statement —
    // but returning `None` for it keeps the "is this a comment with something in it" question
    // honest.
    if rest.is_empty() {
        return None;
    }
    Some(rest)
}

/// Whether a file declares itself irreversible.
///
/// Separate from [`extract_down`] so a caller can ask the two questions in the order it needs
/// them: "is this declared irreversible" is about the author's intent and "does a reversal exist"
/// is about the file's contents, and the upgrade plan needs the first while the gate needs the
/// second.
#[must_use]
pub fn declares_no_down(content: &str) -> bool {
    content
        .lines()
        .any(|line| comment_body(line).is_some_and(|body| body.trim() == NO_DOWN_MARKER))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file shaped exactly like `0199_deployment_tooling.sql`: prose that says "Down script"
    /// twice, the reversal indented, and no marker. Under a heading-based parser this would be
    /// the hardest file in the tree to read; under this one it is the easiest.
    const REAL_SHAPED: &str = r#"
-- The deployment centre's release surface.
-- ## Down script
--
-- The reversal is written as executable statements rather than as a second file.

create table release_manifests (version text primary key);

-- ## Down script
--
-- Reverse order, children before parents.

-- omnion:down
--   drop table if exists release_manifests;

-- Commented out, like every other migration in this tree.
"#;

    /// The shape this repository actually writes, taken from `0162_reliability.sql` verbatim.
    const HEADED_SHAPED: &str = r#"
-- ---------------------------------------------------------------------------
-- Down script (docs/05-VERSIONING.md)
-- ---------------------------------------------------------------------------
-- Reverse order, children before parents, so no foreign key is left pointing at a dropped
-- table. `if exists` throughout.

--   drop table if exists intake_rejections;
--   drop table if exists intake_endpoints;

-- Commented out, like every other migration in this tree, because the up half is run by
-- `Db::migrate` and a reversal written as live statements would be executed by it too.
"#;

    #[test]
    fn a_headed_block_yields_its_indented_statements() {
        // This is the load-bearing shape: the marker variant is what the module doc argued for and
        // NO file in this tree uses it. A parser that reads only the marker reports `has_down =
        // false` for all 58 migrations, and the gate that trusts it refuses to apply any of them.
        let script = extract_down(HEADED_SHAPED);
        assert_eq!(
            script.statements,
            vec![
                "drop table if exists intake_rejections;",
                "drop table if exists intake_endpoints;"
            ],
            "the indented comment lines after the heading are the statements"
        );
        assert!(script.has_statements());
        assert_eq!(script.block_start, Some(3), "1-based, the heading line");
    }

    #[test]
    fn a_double_hash_heading_is_the_same_heading() {
        // `0037_metric_catalog.sql` and `0040_observability_tracing.sql` write `-- ## Down script`.
        assert_eq!(
            extract_down("-- ## Down script\n--   drop table a;").statements,
            vec!["drop table a;"]
        );
    }

    #[test]
    fn prose_that_mentions_a_down_script_does_not_open_a_block() {
        // Every one of these is a real line in this repository, and each of them would open a block
        // under a substring match — which then swallows every later indented comment in the file,
        // including another section's explanation.
        let prose = [
            // `0035_observability_logs.sql`
            "-- The down script exists as a comment rather than as a second file because the \
             migration runner",
            // `0207_migration_safety.sql`, inside a `create table`
            "    -- `false` means \"no down script was found in the file\", which is NOT the same as \
             \"the down",
            // A sentence that ends with the phrase
            "-- read docs/05-VERSIONING.md: Down script",
        ];
        for line in prose {
            assert!(
                !is_down_heading(line.trim_start_matches('-')),
                "prose must not open a block: {line}"
            );
            let script = extract_down(&format!("{line}\n--   drop table a;\n"));
            assert!(
                script.statements.is_empty(),
                "no statements were claimed from prose: {line} -> {script:?}"
            );
        }
    }

    #[test]
    fn a_heading_with_a_trailing_sentence_is_not_a_heading() {
        assert!(!is_down_heading("Down script, but written live"));
        assert!(!is_down_heading("Down scripts"));
        assert!(is_down_heading("Down script"));
        assert!(is_down_heading("Down script (docs/05-VERSIONING.md)"));
        assert!(is_down_heading("## Down script"));
    }

    /// The test that decides this crate's central claim.
    ///
    /// The module doc asserts that the parser reads the reversals this repository actually
    /// writes. That is a claim about FILES, and the file that proves it is not a fixture: a
    /// hand-written fixture shaped like `0162_reliability.sql` agrees with the parser by
    /// construction, which is exactly the "documented but unreachable" failure this request is
    /// written against. So the real files are read, and the assertion is a COUNT.
    #[test]
    fn the_trees_own_migrations_are_read_as_they_are_written() {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../database/migrations");
        let mut files = std::fs::read_dir(&dir)
            .expect("the migrations directory is embedded, so it is on disk")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "sql"))
            .collect::<Vec<_>>();
        files.sort();
        assert!(
            files.len() > 50,
            "the tree grew: the assertion is about the whole set, not a fixture ({} files)",
            files.len()
        );

        let mut with_reversal = 0;
        let mut headings_without_statements: Vec<String> = Vec::new();
        for path in &files {
            let content = std::fs::read_to_string(path).expect("readable");
            let script = extract_down(&content);
            if script.has_statements() {
                with_reversal += 1;
            } else if content
                .lines()
                .any(|line| comment_body(line).is_some_and(is_down_heading))
            {
                // A heading with no statement under it is a half-written reversal. Naming the
                // files is the point: this list is short and every entry is a real defect.
                headings_without_statements.push(
                    path.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }

        assert!(
            headings_without_statements.is_empty(),
            "a down-script heading with no statements under it is a reversal nobody can run: \
             {headings_without_statements:?}"
        );
        // SEVEN is the measured truth, and the number is the point rather than a threshold to be
        // relaxed until the suite is green: an independent scan of the same 61 files (heading or
        // marker, then indented comment lines) finds exactly these seven with these statement
        // counts — `0037`, `0040`, `0162`, `0199`, `0207`, `0216` and `0221`. A parser that reads
        // only its own marker finds ZERO, and a parser that opened on any line mentioning "down
        // script" finds eleven blocks, five of which are prose.
        //
        // The other 54 migrations have no reversal at all, which is a fact about the repository
        // and not about this parser. It is also exactly why the policy's answer for them is a
        // WAIVER and not a pass: a migration with no reversal is "take the backup".
        assert_eq!(
            with_reversal,
            7,
            "the parser must read exactly the files that carry a reversal; it read {with_reversal} \
             of {total} files. New migration with a reversal? Raise this number WITH the file.",
            total = files.len()
        );
    }

    #[test]
    fn a_marked_block_yields_its_indented_statements() {
        let script = extract_down(REAL_SHAPED);
        assert_eq!(
            script.statements,
            vec!["drop table if exists release_manifests;"],
            "the indented comment lines inside the marker are the statements"
        );
        assert!(script.has_statements());
    }

    #[test]
    fn the_phrase_down_script_in_prose_is_not_a_block() {
        // The file above contains "## Down script" twice. A parser keyed on the phrase would
        // open a block at the first one and then have to guess. This test pins the property that
        // makes that guess unnecessary: the statements come from the MARKER, so the two mentions
        // of the phrase in the leading comment are inert text.
        let without_marker = REAL_SHAPED.replace("omnion:down", "omnion:down-note");
        let script = extract_down(&without_marker);
        assert!(
            script.statements.is_empty(),
            "prose alone is not a reversal: {script:?}"
        );
    }

    #[test]
    fn commentary_inside_the_block_is_dropped_and_indent_is_what_marks_a_statement() {
        let content = r#"
-- omnion:down
-- Reverse order, children before parents, `if exists` throughout.
--
--   drop table if exists child;
--
--   drop table if exists parent;
-- Done.
"#;
        let script = extract_down(content);
        assert_eq!(
            script.statements,
            vec![
                "drop table if exists child;",
                "drop table if exists parent;"
            ],
            "unindented commentary is dropped; indented lines are kept, blank lines separate them"
        );
    }

    #[test]
    fn an_unmarked_reversal_is_prose_and_reports_no_down() {
        // The failure mode the request is written against: a migration that *talks* about a
        // reversal must not pass the gate. `has_statements() == false` routes it to the policy
        // check, which either demands a waiver or routes it to "take the backup" — and both of
        // those are correct, which a silent pass would not be.
        let content = "-- ## Down script\n--\n-- We should drop the tables one day.\n";
        let script = extract_down(content);
        assert!(!script.has_statements());
        assert_eq!(
            script.sql(),
            None,
            "an empty reversal must not be executable"
        );
    }

    #[test]
    fn a_missing_block_is_not_an_error_and_has_no_start_line() {
        let script = extract_down("create table t (id int);");
        assert!(!script.has_statements());
        assert!(script.block_start.is_none());
        assert!(!script.declared_no_down);
    }

    #[test]
    fn the_marker_closes_the_block_so_later_prose_is_not_swallowed() {
        let content = r#"
-- omnion:down
--   drop table if exists a;
-- omnion:down
--   drop table if exists b;
--   drop table if exists c;
"#;
        let script = extract_down(content);
        assert_eq!(
            script.statements,
            vec!["drop table if exists a;"],
            "a second marker ends the first block; its own statements belong to no block"
        );
    }

    #[test]
    fn a_declared_no_down_marker_is_read_whether_or_not_a_reversal_exists() {
        assert!(declares_no_down("-- omnion:no-down\ncreate table t ();"));
        assert!(!declares_no_down("-- omnion:down\n--   drop table t;"));
        // The marker is matched as a whole comment line, so prose that merely NAMES it does not
        // count. Without this, a migration explaining the convention would declare itself
        // irreversible and route every upgrade through the backup path.
        assert!(
            !declares_no_down(
                "-- see the omnion:no-down marker described in docs/05-VERSIONING.md"
            ),
            "prose naming the marker is not the marker"
        );
    }

    #[test]
    fn a_file_with_a_reversal_and_a_no_down_marker_is_reported_as_both() {
        // They contradict each other and the platform does not resolve the contradiction: the
        // author's declaration drives the upgrade plan's verdict, the file's contents drive the
        // gate, and a human resolving that is better than a parser picking a winner.
        let content = "-- omnion:no-down\n-- omnion:down\n--   drop table t;";
        let script = extract_down(content);
        assert!(script.declared_no_down);
        assert!(script.has_statements());
    }

    #[test]
    fn the_block_start_is_the_markers_line_so_an_error_can_point_at_it() {
        let script = extract_down("line one\nline two\n-- omnion:down\n--   drop table t;");
        assert_eq!(script.block_start, Some(3), "1-based, the marker line");
    }
}
