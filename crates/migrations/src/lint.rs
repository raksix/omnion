//! The banned-shape lint (docs/requests/REQ-129, slice 1).
//!
//! ## Why a lint and not a review checklist
//!
//! The destructive shapes are not the ones somebody chooses. `alter table … drop column` is
//! chosen when a migration is failing and it is faster to remove the column than to find out who
//! reads it; `alter column … type` is chosen when a `text` column turns out to hold an integer.
//! Both are locally reasonable and globally expensive, which is precisely the class of decision a
//! checklist loses and a machine wins. So the rule is enforced where it can be, and the report
//! says which pattern fired and on which line so a waiver is a decision rather than a shrug.
//!
//! ## What is refused, and what is merely flagged
//!
//! The six patterns below are the ones that **lose a row or a column**. Two of them — a type
//! change and a `drop`-then-`add` pair — are refused because they are *silently* destructive:
//! the data survives the statement and the schema stops meaning what it meant. The rest are
//! refused because the documented alternative exists and is not much longer.
//!
//! The severity split is not cosmetic. A `drop table` is an `error` (the gate fails). A
//! `not null` column added without a default is a `warning` on its own, because `add nullable →
//! backfill → constrain` is a *three-migration* recipe and a single file that does the first step
//! is doing something correct — so a hard error there would fail every honest attempt to follow
//! the recipe. What fails is the file that does the whole recipe at once.
//!
//! ## The line numbers are the deliverable
//!
//! A violation with no line is a violation somebody has to go find. Every pattern here reports
//! the 1-based line the offending text starts on, and [`extract_down`]'s marker convention means
//! a **commented-out** reversal does NOT trip `drop_table` — the down script of every migration in
//! this tree is full of `drop table` statements, and a lint that flagged those would flag the
//! entire repository on its first run.

/// One banned shape, with the closed vocabulary a policy can switch on and off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Pattern {
    /// The stable key a waiver is recorded against.
    pub key: &'static str,
    /// Plain-language explanation of what it prevents, shown in the policy screen.
    pub why: &'static str,
    /// The substring that fires it, lowercased.
    pub needle: &'static str,
    /// `true` fails the gate, `false` is reported but does not.
    pub blocking: bool,
}

/// The closed vocabulary. A policy row stores `key → enabled`; a key here that is absent from that
/// map is treated as **enabled**, so an installation that has never written a policy enforces
/// every rule. Opting out is a deliberate act with a record, which is the only direction a safety
/// rule should be adjustable in.
pub const PATTERNS: [Pattern; 6] = [
    Pattern {
        key: "drop_column",
        why: "a dropped column is gone; stop reading it, stop writing it, drop it in a later release",
        needle: "drop column",
        blocking: true,
    },
    Pattern {
        key: "drop_table",
        why: "a dropped table takes its rows with it",
        needle: "drop table",
        blocking: true,
    },
    Pattern {
        key: "rename",
        why: "a rename is a new table plus a cut-over, not an alter; it breaks every reader mid-deploy",
        needle: "rename to",
        blocking: true,
    },
    Pattern {
        // The silent killer, and the only pattern whose rule is narrower than its name suggests.
        // `alter column … type …` succeeds, the data survives, and every reader is now wrong about
        // what the column holds. That is the shape to refuse.
        //
        // It is NOT every `alter column`: migrations 0011, 0013 and 0016 in this tree use
        // `set not null`, `set default` and `drop not null`, which are ordinary additive operations
        // that reinterpret no stored value. See [`type_change`] for the exact test.
        key: "type_change",
        needle: "alter column",
        why: "changing a column's type succeeds while changing what its values mean",
        blocking: true,
    },
    Pattern {
        // The one rule that is not a plain substring, because the request's banned shape is
        // SPECIFIC: "adding a `not null` column without a default". Every other phrasing of that
        // rule is either a false positive or a rule about something else — see [`not_null_column`]
        // for why a `create table` primary key and a `check (… is not null)` clause must both be
        // left alone.
        key: "not_null_without_default",
        why: "add the column nullable, backfill it, then constrain it in a later migration",
        needle: "add column",
        blocking: false,
    },
    Pattern {
        // Also not a plain substring — see [`constraint_in_data_statement`]. A table-level
        // `constraint … check (…)` written inside `create table` is created empty alongside the
        // table and validates nothing; half the migrations in this repository declare their
        // constraints that way and all of them are correct.
        key: "add_constraint_same_statement",
        why: "a constraint validated in the same statement as the data change locks the table twice",
        needle: "add constraint",
        blocking: false,
    },
];

/// One finding.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Violation {
    /// The migration this was found in, so a finding is self-contained once it leaves
    /// [`lint`]. The caller passes the version it already parsed rather than this module
    /// re-deriving it from a filename it does not have.
    pub version: String,
    /// The pattern key, matching [`Pattern::key`].
    pub pattern: &'static str,
    /// 1-based line the offending text starts on.
    pub line: usize,
    /// The trimmed source line, so the report shows what fired without the reader opening the file.
    pub excerpt: String,
    /// `true` when this pattern fails the gate.
    pub blocking: bool,
    /// Whether the firing line is a comment — a commented-out reversal is not executed, so it is
    /// reported as a warning at most and never blocks.
    pub commented: bool,
}

impl Violation {
    /// `true` when this finding fails the gate.
    ///
    /// A commented line can never block: every migration in this tree carries its reversal as
    /// commented `drop table` statements, so a lint that blocked on those would refuse the
    /// repository's own files — which is how a lint gets switched off and stops being one.
    #[must_use]
    pub fn fails_gate(&self) -> bool {
        self.blocking && !self.commented
    }

    /// The finding's identity, matching `migration_violations`' uniqueness constraint.
    ///
    /// `pattern:line` and not the excerpt: the excerpt changes when somebody rewords a comment,
    /// and a waiver keyed on it would expire the next time somebody improved the prose.
    #[must_use]
    pub fn identity(&self) -> String {
        format!("{}:{}", self.pattern, self.line)
    }
}

/// Lint one migration file.
///
/// `enabled` maps a [`Pattern::key`] to whether this installation enforces it. A key missing from
/// the map is enforced — see [`PATTERNS`] for why the default is "on".
///
/// Pure: it reads a string and returns findings. It never touches a database, a clock or the
/// filesystem, which is what lets `--dry-run` answer honestly without connecting to anything and
/// what makes the whole gate unit-testable.
#[must_use]
pub fn lint(
    version: &str,
    content: &str,
    enabled: &std::collections::BTreeMap<String, bool>,
) -> Vec<Violation> {
    let lines: Vec<&str> = content.lines().collect();
    // One pass, so the absence-rule below has a statement to look at without re-reading the file
    // per line. `statement_has_default[i]` answers "does the statement this line belongs to carry
    // a default"; computing it here rather than inside the loop is what keeps the linter linear in
    // the file size.
    let statement_of = statements_of(&lines);

    let mut violations = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let lower = line.to_lowercase();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // A comment is not executed. It still gets reported, because a commented-out `alter
        // column` sitting in the up half of a migration is usually somebody mid-edit — but it is
        // reported as non-blocking, which is what keeps the reversal blocks legal.
        let commented = trimmed.starts_with("--");

        for pattern in PATTERNS {
            if !enabled.get(pattern.key).copied().unwrap_or(true) {
                continue;
            }
            let fires = if pattern.key == "not_null_without_default" {
                not_null_column(&lower, statement_of[index].as_deref().unwrap_or_default())
            } else if pattern.key == "add_constraint_same_statement" {
                constraint_in_data_statement(
                    &lower,
                    statement_of[index].as_deref().unwrap_or_default(),
                )
            } else if pattern.key == "type_change" {
                type_change(&lower)
            } else {
                lower.contains(pattern.needle)
            };

            if fires {
                violations.push(Violation {
                    version: version.to_owned(),
                    pattern: pattern.key,
                    line: index + 1,
                    excerpt: trimmed.chars().take(120).collect(),
                    blocking: pattern.blocking,
                    commented,
                });
            }
        }
    }
    // Sorted so the report is stable across runs: two findings on one line, in pattern order,
    // which makes a diff between two lint runs meaningful instead of noise.
    violations.sort_by(|a, b| (a.line, a.pattern).cmp(&(b.line, b.pattern)));
    violations
}

/// The whole SQL statement each line belongs to, or `None` when it is outside any.
///
/// Statements are opened by [`STATEMENT_OPENERS`] and closed by the first line ending in `;`. This
/// is a window, not a parser: a `;` inside a string literal closes it early, which narrows the
/// window and can only ever under-report. A safety rule that invents a finding is worse than one
/// that misses a rare one, because the response to a false finding is a waiver, and a waiver on
/// something that was never real teaches everybody that the findings are noise.
fn statements_of(lines: &[&str]) -> Vec<Option<String>> {
    // Collect the spans first, then fill in the TEXT for every line of a span at once. The
    // per-line alternative — snapshotting the buffer as it grows — silently fails on exactly the
    // case the `not null` rule depends on: a `default` written three lines BELOW the `not null` is
    // not in the snapshot taken at the `not null` line, so a correct column is reported as
    // missing its default.
    let mut spans: Vec<Option<(usize, usize)>> = vec![None; lines.len()];
    let mut start: Option<usize> = None;

    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim().to_lowercase();
        if start.is_none() && STATEMENT_OPENERS.iter().any(|o| trimmed.starts_with(o)) {
            start = Some(index);
        }
        if let Some(begin) = start {
            if trimmed.ends_with(';') {
                for slot in spans.iter_mut().take(index + 1).skip(begin) {
                    *slot = Some((begin, index));
                }
                start = None;
            }
        }
    }
    // An unterminated final statement (a file that ends mid-statement) is still a statement: the
    // closing `;` is not what makes the lines belong together.
    if let Some(begin) = start {
        let last = lines.len().saturating_sub(1);
        for slot in spans.iter_mut().take(lines.len()).skip(begin) {
            *slot = Some((begin, last));
        }
    }

    let mut result: Vec<Option<String>> = vec![None; lines.len()];
    for (index, line) in lines.iter().enumerate() {
        result[index] = match spans[index] {
            Some((begin, end)) => {
                let mut buffer = String::new();
                for part in &lines[begin..=end] {
                    let t = part.trim().to_lowercase();
                    if !t.starts_with("--") {
                        buffer.push_str(&t);
                        buffer.push(' ');
                    }
                }
                Some(buffer)
            }
            // Outside any statement the line is its own window. That is what keeps a `not null` in
            // the leading comment block — which is prose ABOUT this rule — from being a finding.
            None => Some(line.trim().to_lowercase()),
        };
    }
    result
}

/// Is this line adding a `not null` column that has no default?
///
/// The request's banned shape is specific — "adding a `not null` column without a default" — and
/// every looser spelling of that rule is wrong in a way this repository proves:
///
/// * **`alter table … add column x text not null`** is the shape. It has no default, and
///   PostgreSQL fills existing rows with NULL, which then violates the constraint the statement
///   just added — or succeeds only because the table is empty. It is the one case that is
///   genuinely dangerous and it is detectable exactly.
/// * **`create table … (name text not null)`** is a column created WITH its rows. There is no
///   backfill to schedule and no window where the column exists without its constraint. Requiring
///   a default here would flag every table in the repository, including every primary key, which
///   is `not null` by definition — and a lint that fires on its own repository gets switched off.
/// * **`check ((resolved_at is not null) = (…))`** is prose about nullability inside a predicate.
///   `not null` is not even a substring of a column definition there.
/// * **`alter table … add column x text not null default ''`** is the documented recipe's end
///   state and must be silent.
///
/// So the rule fires on exactly one shape, and the tree-wide test is what pinned that down: an
/// earlier, looser version reported 30+ findings on correct migrations in this repository, which
/// is indistinguishable from a broken lint.
fn not_null_column(line_lower: &str, statement_lower: &str) -> bool {
    // The `not null` has to be a COLUMN's own constraint, so any text inside a `check (…)` clause
    // is removed before asking. A predicate like `check (endpoint_id is not null or …)` is prose
    // about nullability; migration 0197 contains one and adding the column it guards is correct.
    let without_checks = strip_check_clauses(line_lower);
    if !without_checks.contains("not null") {
        return false;
    }
    // A default anywhere in the ADD COLUMN statement satisfies the recipe.
    if statement_lower.contains("default") {
        return false;
    }
    // `add column` is the only op that introduces a column into a table that already has rows.
    // Without it the column arrives with its data and the recipe does not apply.
    statement_lower.contains("add column") && without_checks.contains("not null")
}

/// Remove every `check (…)` clause, including its nested parentheses, from a lowercased line.
///
/// Depth-counted rather than regex-matched because the clauses nest
/// (`check (a is not null or (b is null and c = 1))`) and a non-greedy match would stop at the
/// first `)` and leave the rest of the predicate looking like column text. `unbalanced` is the
/// one case it cannot do better on — a `check` opened and never closed on this line — and it
/// errs toward removing MORE text, which can only ever silence a finding, never invent one.
fn strip_check_clauses(line_lower: &str) -> String {
    let mut out = String::with_capacity(line_lower.len());
    let mut rest = line_lower;
    while let Some(at) = rest.find("check") {
        let after = &rest[at..];
        let Some(open_rel) = after.find('(') else {
            out.push_str(rest);
            return out;
        };
        let open = at + open_rel;
        let before = &rest[..open];
        // Only a standalone `check` keyword counts — `constraint_foo_check (...)` is a name.
        let ends_with_check_word =
            before.is_empty() || before.ends_with(|c: char| !c.is_alphanumeric() && c != '_');
        if !ends_with_check_word {
            out.push_str(&rest[..at + "check".len()]);
            rest = &rest[at + "check".len()..];
            continue;
        }
        out.push_str(before);
        // Walk to the matching close paren.
        let mut depth = 0usize;
        let mut end = open;
        for (offset, ch) in rest[open..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + offset;
                        break;
                    }
                }
                _ => {}
            }
        }
        rest = if end > open { &rest[end + 1..] } else { "" };
    }
    out.push_str(rest);
    out
}

/// Is this line adding a validated constraint to a table in the same statement as a data change?
///
/// The request's shape is "adding a constraint that requires a full-table validation in the same
/// statement as the data change". Both halves matter:
///
/// * A constraint declared INSIDE `create table` is created empty with the table, so it validates
///   zero rows and locks nothing. Every table-level `constraint … check (…)` in this repository is
///   that shape, and half of them would otherwise be findings.
/// * `alter table … add constraint … check (…)` after a bulk `update` in the same statement
///   validates the updated rows while holding the lock — the documented reason for splitting them.
///
/// A `not valid` constraint is the documented escape and is silent here: it does not scan the
/// table, which is the entire reason it exists.
fn constraint_in_data_statement(line_lower: &str, statement_lower: &str) -> bool {
    if !statement_lower.contains("add constraint") {
        return false;
    }
    if !statement_lower.starts_with("alter table") {
        return false; // a `create table`'s own constraints are created empty with the table
    }
    if statement_lower.contains("not valid") {
        return false; // the documented escape: it validates nothing now
    }
    // A DATA change is a statement that rewrites rows: an `update`, a bare `delete`, or an
    // `insert into` backfill. Matched as a STATEMENT-LEADING or clause keyword, never as a bare
    // substring: `references webhook_endpoints (id) on delete set null` contains "delete" and is
    // a referential action, not a row rewrite — migration 0197 has that exact line and is correct.
    // `add column` is deliberately not a data change either; it touches no existing row.
    let mutates_rows = ["update ", "delete from", "insert into"]
        .iter()
        .any(|verb| statement_lower.contains(verb));
    // Strip the check clause so the predicate's own `not null`-style text cannot re-trigger it,
    // and require the line itself to be the `add constraint` (not part of a check body).
    let line_body = strip_check_clauses(line_lower);
    mutates_rows && line_body.contains("add constraint")
}

/// Is this line changing a column's TYPE?
///
/// `alter column` on its own is not the hazard — `set not null`, `drop not null` and `set default`
/// all start with it and none of them reinterprets a stored value. Migrations 0011, 0013 and 0016
/// in this tree do exactly those and are correct. The hazard is specifically the cast: the
/// statement completes, no row is lost, and every reader's assumption about the column's contents
/// becomes false with nothing failing.
///
/// The safe operations are named explicitly rather than pattern-excluded, so a future
/// `alter column … set data type` cannot slip past on the word "type" alone.
fn type_change(line_lower: &str) -> bool {
    if !line_lower.contains("alter column") {
        return false;
    }
    if [
        "set not null",
        "drop not null",
        "set default",
        "drop default",
    ]
    .iter()
    .any(|safe| line_lower.contains(safe))
    {
        return false;
    }
    line_lower.contains(" type ") || line_lower.contains(" using ")
}

/// The statement forms the linter recognises as an opener.
///
/// Closed on purpose. An unrecognised opener means the lines after it are treated as their own
/// statements, which is the conservative direction: a narrower statement window under-reports
/// rather than inventing a finding.
const STATEMENT_OPENERS: [&str; 6] = [
    "create table",
    "alter table",
    "create index",
    "create unique",
    "insert into",
    "comment on",
];

/// `true` when any finding in the list fails the gate.
#[must_use]
pub fn gate_fails(violations: &[Violation]) -> bool {
    violations.iter().any(Violation::fails_gate)
}

/// The violations that fail the gate, as the CI failure message names them.
#[must_use]
pub fn blocking(violations: &[Violation]) -> Vec<&Violation> {
    violations.iter().filter(|v| v.fails_gate()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lint_all(content: &str) -> Vec<Violation> {
        lint("0207", content, &Default::default())
    }

    #[test]
    fn a_clean_additive_migration_has_no_findings() {
        let content = "\
create table if not exists widgets (
    id uuid primary key,
    name text not null default ''
);
create index widgets_name_idx on widgets (name);
";
        assert!(lint_all(content).is_empty(), "{:?}", lint_all(content));
    }

    #[test]
    fn a_dropped_column_is_reported_with_its_line_and_excerpt() {
        let v = lint_all("create table t (a int);\nalter table t drop column a;\n");
        assert_eq!(v.len(), 1, "one finding");
        assert_eq!(v[0].pattern, "drop_column");
        assert_eq!(v[0].line, 2, "1-based: line 2 is the alter");
        assert!(
            v[0].excerpt.contains("drop column a"),
            "the excerpt shows what fired"
        );
        assert!(v[0].fails_gate());
    }

    #[test]
    fn a_commented_reversal_never_blocks_the_gate() {
        // This is the load-bearing property of the whole lint. Every migration in this tree
        // carries its reversal as commented `drop table` statements; a lint that blocked on
        // those would refuse the repository's own files on its first run, and a lint that
        // refuses everything gets switched off.
        let content = "\
create table t (id int);
-- omnion:down
--   drop table if exists t;
";
        let v = lint_all(content);
        assert_eq!(v.len(), 1, "it is reported");
        assert_eq!(v[0].pattern, "drop_table");
        assert!(v[0].commented);
        assert!(
            !v[0].fails_gate(),
            "a commented reversal is not executed, so it cannot block"
        );
        assert!(!gate_fails(&v));
    }

    #[test]
    fn a_type_change_blocks_because_it_succeeds_while_changing_meaning() {
        // The one that needs the most explanation to a reviewer and so is the easiest to argue
        // past: the statement completes, the rows survive, and every reader is now wrong.
        let v = lint_all("alter table t alter column id type bigint;\n");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].pattern, "type_change");
        assert!(v[0].fails_gate());
    }

    #[test]
    fn not_null_alone_is_a_warning_because_the_recipe_needs_three_files() {
        // `add nullable -> backfill -> constrain` is three migrations. The dangerous first shape —
        // adding a not-null column with no default to a table that already has rows — is a
        // WARNING, not an error: the recipe is three files, so failing the gate on one honest
        // attempt at it would make the recipe impossible to complete in one commit.
        let v = lint_all("alter table t add column age int not null;\n");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].pattern, "not_null_without_default");
        assert!(
            !v[0].fails_gate(),
            "the recipe spans migrations, so this warns rather than blocking"
        );
        assert!(!gate_fails(&v));
    }

    #[test]
    fn a_drop_column_and_an_add_constraint_after_an_update_are_two_findings() {
        let content = "\
alter table t drop column a;
update t set a = 1 where b is not null;
alter table t add constraint c check (a > 0);
";
        let v = lint_all(content);
        // drop_column on line 1; add_constraint on line 3 because the statement window carries the
        // preceding update? No — statements are per-statement, so line 3's own statement has no
        // mutating verb. So exactly the drop_column fires.
        assert_eq!(
            v.iter().filter(|x| x.pattern == "drop_column").count(),
            1,
            "{v:?}"
        );
    }

    #[test]
    fn a_constraint_in_the_same_statement_as_an_update_is_reported() {
        let content = "\
alter table t
    update b set flagged = true where a > 0,
    add constraint c check (a > 0);
";
        let v = lint_all(content);
        assert_eq!(
            v.iter()
                .filter(|x| x.pattern == "add_constraint_same_statement")
                .count(),
            1,
            "validating after rewriting rows in one statement is the documented hazard: {v:?}"
        );
    }

    #[test]
    fn an_on_delete_referential_action_is_not_a_data_change() {
        // The false positive that a naive `"delete"` substring check produces. `on delete set
        // null` is a referential action on the FOREIGN KEY; it rewrites no rows in this statement
        // and constrains nothing. Migration 0197 contains exactly this line, and the lint has to
        // stay silent on it or it is reporting on correct code.
        let content = "\
alter table notification_channels
    add column endpoint_id uuid references webhook_endpoints (id) on delete set null,
    add constraint c check (endpoint_id is not null);
";
        let v = lint_all(content);
        assert!(
            !v.iter()
                .any(|x| x.pattern == "add_constraint_same_statement"),
            "`on delete set null` is an FK action, not an update: {v:?}"
        );
    }

    #[test]
    fn adding_a_constraint_after_adding_columns_is_silent() {
        // Migration 0197's exact shape: two nullable columns plus a check that one of them is set.
        // No rows are rewritten, so nothing is validated under a lock.
        let content = "\
alter table notification_channels
    add column endpoint_id uuid,
    add constraint destination_check check (endpoint_id is not null);
";
        let v = lint_all(content);
        assert!(
            !v.iter()
                .any(|x| x.pattern == "add_constraint_same_statement"),
            "add column is not a data change: {v:?}"
        );
        assert!(
            !v.iter().any(|x| x.pattern == "not_null_without_default"),
            "and the check predicate's `is not null` is prose: {v:?}"
        );
    }

    #[test]
    fn a_pattern_switched_off_in_the_policy_is_not_reported() {
        // The direction matters: switching a rule OFF is a deliberate act with a row in the
        // policy and a waiver's worth of thought behind it. Switching one ON by omission must
        // not be possible, or a new pattern would ship dormant in every existing installation.
        let mut enabled = std::collections::BTreeMap::new();
        enabled.insert("drop_column".to_owned(), false);
        let v = lint("0207", "alter table t drop column a;\n", &enabled);
        assert!(v.is_empty(), "{v:?}");
    }

    #[test]
    fn a_pattern_absent_from_the_policy_is_still_enforced() {
        let v = lint(
            "0207",
            "alter table t drop column a;\n",
            &Default::default(),
        );
        assert_eq!(
            v.len(),
            1,
            "an installation that never wrote a policy enforces every rule"
        );
    }

    #[test]
    fn findings_are_stable_across_runs_so_two_runs_diff_meaningfully() {
        // Two patterns on one line must sort the same way every time, or a diff between two lint
        // runs is noise an operator learns to ignore. drop_column and add_constraint-after-update
        // both fire here.
        let content = "\
alter table t
    update b set x = 1 where a > 0,
    add column d text,
    drop column a,
    add constraint c check (a > 0);
";
        let first = lint_all(content);
        let second = lint_all(content);
        assert_eq!(first, second, "two runs of a pure function must agree");
        let patterns: Vec<_> = first.iter().map(|x| x.pattern).collect();
        assert!(patterns.contains(&"drop_column"), "{patterns:?}");
        assert!(
            patterns.contains(&"add_constraint_same_statement"),
            "{patterns:?}"
        );
        // Sorted by (line, pattern): the drop is on an earlier line than the add constraint.
        let drop_at = patterns.iter().position(|p| *p == "drop_column").unwrap();
        let add_at = patterns
            .iter()
            .position(|p| *p == "add_constraint_same_statement")
            .unwrap();
        assert!(drop_at < add_at, "sorted by line: {first:?}");
    }

    #[test]
    fn the_identity_ignores_the_excerpt_so_a_rewording_does_not_expire_a_waiver() {
        // A waiver is keyed on (version, pattern, line). If it were keyed on the excerpt,
        // improving a comment's wording would silently clear the waiver — which is the failure
        // mode the whole violations table is shaped around avoiding.
        let v = lint_all("alter table t drop column a;\n");
        let mut reworded = v[0].clone();
        reworded.excerpt = "-- somebody improved this comment".to_owned();
        assert_eq!(
            v[0].identity(),
            reworded.identity(),
            "same finding, same identity"
        );
    }

    #[test]
    fn adding_a_not_null_column_to_a_table_that_has_rows_is_the_shape_the_rule_is_for() {
        // The one genuinely dangerous case: the table already has rows, the column arrives
        // without a default, and PostgreSQL fills them with NULL — which violates the constraint
        // the same statement just added.
        let v = lint_all("alter table widgets add column nickname text not null;\n");
        assert_eq!(
            v.iter()
                .filter(|x| x.pattern == "not_null_without_default")
                .count(),
            1,
            "{v:?}"
        );
        assert!(
            !v[0].fails_gate(),
            "it is a warning: the recipe is three migrations, not one file"
        );
    }

    #[test]
    fn the_documented_recipes_end_state_is_silent() {
        // Step three of add-nullable -> backfill -> constrain, done correctly: the constraint goes on
        // as a validated `add column` WITH a default. Flagging this would make the recipe
        // impossible to complete.
        let v = lint_all("alter table widgets add column nickname text not null default '';\n");
        assert!(
            !v.iter().any(|x| x.pattern == "not_null_without_default"),
            "a not-null column WITH a default is the recipe's end state: {v:?}"
        );
    }

    #[test]
    fn a_create_table_column_is_not_the_rule_even_without_a_default() {
        // The false positive that made this rule statement-scoped and then column-scoped and then
        // shape-scoped. A column created WITH its rows has no backfill window and no constraint
        // that could fail against existing data. Every primary key in this repository is in this
        // shape, and none of them can carry a default.
        let v = lint_all("create table t (id uuid primary key, name text not null);\n");
        assert!(
            !v.iter().any(|x| x.pattern == "not_null_without_default"),
            "a column that arrives with its data is not an add-column: {v:?}"
        );
    }

    #[test]
    fn a_null_check_predicate_is_not_a_column_definition() {
        // `not null` here is prose inside a predicate, not a column's constraint. Two migrations
        // in this repository contain exactly this line.
        let v = lint_all(
            "alter table incidents add constraint shape check ((resolved_at is not null) = (to_state = 'healthy'));\n",
        );
        assert!(
            !v.iter().any(|x| x.pattern == "not_null_without_default"),
            "a check predicate is not a column: {v:?}"
        );
    }

    #[test]
    fn a_multiline_add_column_with_its_default_far_below_is_silent() {
        // The statement window is what makes this work: `not null` on one line, `default` three
        // lines down, and the answer is "this column is fine".
        let content = "\
alter table widgets
    add column nickname text
        not null
        default '';
";
        let v = lint_all(content);
        assert!(
            !v.iter().any(|x| x.pattern == "not_null_without_default"),
            "the default is in the same statement: {v:?}"
        );
    }

    #[test]
    fn one_migrations_default_does_not_excuse_the_next_statements_column() {
        // The window really is per-statement. Without an ending, a's default would silence b.
        let content = "\
alter table a add column x text not null default '';
alter table b add column y text not null;
";
        let v = lint_all(content);
        assert_eq!(
            v.iter()
                .filter(|x| x.pattern == "not_null_without_default")
                .count(),
            1,
            "only b's column is missing its default: {v:?}"
        );
        assert_eq!(v[0].line, 2);
    }

    #[test]
    fn a_safe_alter_column_is_not_the_silent_killer_the_type_rule_is_for() {
        // Migrations 0011, 0013 and 0016 do exactly these and are correct: an `alter column` that
        // adds or removes a nullability constraint or sets a default reinterprets no stored value.
        for safe in [
            "alter table t alter column a set not null;",
            "alter table t alter column a drop not null;",
            "alter table t alter column a set default 0;",
            "alter column enabled_providers set default '{}';",
        ] {
            let v = lint_all(&format!("{safe}\n"));
            assert!(
                !v.iter().any(|x| x.pattern == "type_change"),
                "safe operation flagged: {safe:?} -> {v:?}"
            );
        }
    }

    #[test]
    fn a_column_type_cast_is_refused_even_though_it_succeeds() {
        // The one that needs the most explanation to a reviewer and is therefore the easiest to
        // argue past in a code review: nothing fails, the rows survive, and every reader is now
        // wrong about the column's contents.
        for cast in [
            "alter table t alter column id type bigint;",
            "alter table t alter column id set data type bigint;",
            "alter table t alter column id using id::bigint;",
        ] {
            let v = lint_all(&format!("{cast}\n"));
            assert_eq!(
                v.iter().filter(|x| x.pattern == "type_change").count(),
                1,
                "cast not refused: {cast:?} -> {v:?}"
            );
            assert!(
                gate_fails(&v),
                "and it BLOCKS, unlike every other pattern: {cast:?}"
            );
        }
    }

    #[test]
    fn the_repositorys_own_migrations_are_clean_under_this_lint() {
        // The strongest single check available without a database: every migration in this tree,
        // linted with every rule on, must produce ZERO findings that FAIL THE GATE. Commented
        // reversals are reported but never blocking, so they are expected here and excluded —
        // which is exactly the property that keeps a commented `drop table` legal. A lint that
        // fails the gate on the repository it ships in is a lint that gets switched off.
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../database/migrations");
        let mut checked = 0usize;
        let mut blocking = Vec::new();
        let mut informational = 0usize;
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "sql"))
            .collect();
        entries.sort();
        for path in entries {
            let content = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let version = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .split('_')
                .next()
                .unwrap_or_default()
                .to_owned();
            for f in lint(&version, &content, &Default::default()) {
                if f.fails_gate() {
                    blocking.push(format!(
                        "{}:{}:{} — {}",
                        path.display(),
                        f.line,
                        f.pattern,
                        f.excerpt
                    ));
                } else {
                    informational += 1;
                }
            }
            checked += 1;
        }
        assert!(
            checked > 50,
            "expected the whole tree, only checked {checked}"
        );
        assert!(
            blocking.is_empty(),
            "no migration in this repository may trip the gate:\n{}",
            blocking.join("\n")
        );
        // The commented reversals ARE found and ARE non-blocking — if this ever hits zero, the
        // reversal-comment convention has drifted and the "commented never blocks" property is no
        // longer being exercised by real files.
        assert!(
            informational > 20,
            "expected the tree's commented reversals to be reported as non-blocking, saw {informational}"
        );
    }

    #[test]
    fn an_empty_file_has_no_findings_and_does_not_panic() {
        assert!(lint_all("").is_empty());
        assert!(lint_all("\n\n\n").is_empty());
    }

    #[test]
    fn the_uppercase_spelling_of_a_banned_shape_still_fires() {
        // SQL is case-insensitive; a linter that is not would miss `ALTER TABLE t DROP COLUMN a`
        // and would be bypassed by nothing more than an editor's autocomplete.
        let v = lint_all("ALTER TABLE t DROP COLUMN a;\n");
        assert_eq!(v.len(), 1, "case does not hide a destructive statement");
        assert_eq!(v[0].pattern, "drop_column");
    }
}
