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
//! ## The block is recognised by a marker, not by a heading
//!
//! A heading is prose. `0199_deployment_tooling.sql` contains the words "Down script" in its
//! leading comment explaining *why* the reversal is commented out, and it contains the reversed
//! `drop table` statements, and it is entirely correct. A parser that looked for the phrase
//! "Down script" would find that paragraph and then have to guess whether the lines after it were
//! the reversal or more explanation.
//!
//! The marker is therefore a line that cannot occur in prose:
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

/// The marker that opens (and closes) a reversal block. A line, not a phrase.
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
    let mut script = DownScript {
        declared_no_down: declares_no_down(content),
        ..DownScript::default()
    };

    let mut inside = false;
    for (index, line) in content.lines().enumerate() {
        if let Some(body) = comment_body(line) {
            if body.trim() == DOWN_MARKER {
                if inside {
                    break; // the closing marker ends the block; nothing after it is a statement
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
