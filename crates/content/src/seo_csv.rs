//! Redirect CSV: the round trip between a redirect table and the file an owner hands over
//! (REQ-064, slice 3 — the criterion's import/export half).
//!
//! The criterion named "import/export" and the previous slice shipped only the editor, so this
//! is the half that had to be *built*. Two decisions shape it:
//!
//! * **A parse is a whole-file decision, never a partial import.** A file that is 400 rows and
//!   has one bad line must not write 399 rules: an owner who pastes a spreadsheet into a site
//!   with 400 redirects and reads "imported 399, 1 failed" reasonably concludes the other 399
//!   were saved, and the next audit finds a table nobody meant to change. So every row is parsed
//!   and validated first, every rejection is reported with its line number, and only a file whose
//!   rows *all* pass writes anything. That is what [`RedirectCsv::parse`] returning a plan and
//!   the store executing the plan is for.
//!
//! * **A loop inside the file is caught before the first insert.** `refuse_loop` looks at what is
//!   *in the database*; a CSV can close a circle in one upload (`/a → /b`, `/b → /a`) that no
//!   single row's own check would see, because the first insert is written when the second row
//!   has not been read yet. The plan therefore simulates the whole file against itself first.
//!
//! The dialect is RFC 4180: quoted fields, `""` for a quote inside one, and a quoted field may
//! contain a comma or a newline. What it deliberately does *not* accept is a header the tool
//! cannot place — the column names are pinned, and a file that does not carry them is refused
//! with the names it needs rather than silently importing two empty columns.

use std::collections::BTreeSet;

use crate::error::{ContentError, Result};

/// The column header the importer requires, in order.
///
/// Pinned rather than guessed: an importer that "helpfully" takes the first two columns is how a
/// 2,000-row table ends up as 2,000 rules pointing at the header.
pub const REDIRECT_CSV_HEADER: [&str; 5] = ["from", "to", "status", "pattern", "enabled"];

/// Header aliases the importer accepts, so a file exported from a common redirect plugin loads.
///
/// The *canonical* name is the only one written back on export, which keeps the round trip
/// stable: `{"source", "destination"}` imports, and the next export is `from,to,…`.
const HEADER_ALIASES: [(&str, &[&str]); 5] = [
    (
        "from",
        &["from", "from path", "source", "old", "old path", "old url"],
    ),
    (
        "to",
        &["to", "to path", "destination", "new", "new path", "new url"],
    ),
    ("status", &["status", "status code", "code", "type"]),
    (
        "pattern",
        &["pattern", "match type", "rule type", "wildcard"],
    ),
    ("enabled", &["enabled", "active", "status enabled"]),
];

/// Longest CSV the importer will read.
///
/// A cap rather than a rule, for the same reason `MAX_REDIRECTS_PER_SITE` is a cap: the file is
/// held in memory and split into rows before a single one is written, so the file itself has to
/// have a ceiling. 2 MB is roughly 20,000 rules' worth of the shortest possible row, which is
/// already more than any real table.
pub const MAX_REDIRECT_CSV_BYTES: usize = 2 * 1024 * 1024;

/// How a row asked to be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CsvPattern {
    /// Exact path match.
    Literal,
    /// The platform's own small pattern dialect.
    Pattern,
}

impl CsvPattern {
    /// The word the importer accepts, and the one the exporter writes.
    pub fn as_str(self) -> &'static str {
        match self {
            CsvPattern::Literal => "literal",
            CsvPattern::Pattern => "regex",
        }
    }
}

/// One accepted row, before it has an id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvRedirect {
    /// Path the rule answers.
    pub from_path: String,
    /// Where the request goes.
    pub to_path: String,
    /// 301 or 302.
    pub status_code: i32,
    /// How `from_path` is matched.
    pub pattern: CsvPattern,
    /// Whether the rule is evaluated.
    pub enabled: bool,
}

/// A row the importer refused, with the reason a human can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvRejection {
    /// 1-based line number in the uploaded file, counting the header as line 1.
    pub line: usize,
    /// The row as it was read, for the report.
    pub row: String,
    /// Why it was refused.
    pub reason: String,
}

/// The outcome of parsing a file: what to write, and what was not written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RedirectCsv {
    /// Rows that will be inserted.
    pub accepted: Vec<CsvRedirect>,
    /// Rows that were refused, in file order.
    pub rejected: Vec<CsvRejection>,
    /// Lines skipped as blank or as a comment (`#`).
    pub skipped_lines: usize,
}

impl RedirectCsv {
    /// `true` when the file is safe to execute: nothing was refused.
    pub fn is_clean(&self) -> bool {
        self.rejected.is_empty()
    }

    /// One sentence the panel shows above the result.
    pub fn summary(&self) -> String {
        if self.accepted.is_empty() && self.rejected.is_empty() {
            return String::from("the file has a header and no data rows");
        }
        let mut parts = vec![format!("{} rule(s) ready", self.accepted.len())];
        if !self.rejected.is_empty() {
            parts.push(format!("{} refused", self.rejected.len()));
        }
        if self.skipped_lines > 0 {
            parts.push(format!("{} blank or comment", self.skipped_lines));
        }
        parts.join(", ")
    }
}

/// Parse a redirect CSV into a plan, validating every row before anything is written.
///
/// A row is refused — never repaired — when it cannot be read, and the refusal carries its line
/// number. Blank lines and `#` comments are skipped rather than refused, because a spreadsheet
/// export is full of them and an owner should not have to strip them by hand.
pub fn parse_redirect_csv(input: &str) -> Result<RedirectCsv> {
    if input.len() > MAX_REDIRECT_CSV_BYTES {
        return Err(ContentError::InvalidRedirect(format!(
            "the file is {} bytes; the importer reads up to {MAX_REDIRECT_CSV_BYTES}",
            input.len()
        )));
    }

    let records = split_records(input);
    let mut iter = records.into_iter().enumerate();

    // The header is the first *non-blank* record; a file that opens with a comment is still a
    // file with a header, and refusing it would be a rule nobody can see.
    let header = loop {
        match iter.next() {
            None => {
                return Err(ContentError::InvalidRedirect(
                    "the file is empty — it needs a header row".to_string(),
                ));
            }
            Some((index, line)) if is_blank_or_comment(&line) => {
                let _ = index;
                continue;
            }
            Some((_, line)) => break line,
        }
    };

    let columns = map_header(&split_fields(&header))?;
    let mut plan = RedirectCsv::default();

    for (index, line) in iter {
        let line_no = index + 1;
        if is_blank_or_comment(&line) {
            plan.skipped_lines += 1;
            continue;
        }
        let fields = split_fields(&line);
        // A short row is not a row with empty cells: a file that lost its last column is a
        // different thing from a rule whose `enabled` is blank, and both are refused here
        // because the difference is invisible once the columns have been pulled out by index.
        if fields.len() < columns.iter().filter(|c| c.is_some()).count() {
            plan.rejected.push(CsvRejection {
                line: line_no,
                row: line.clone(),
                reason: format!(
                    "{} columns, expected at least {}",
                    fields.len(),
                    required_columns(&columns)
                ),
            });
            continue;
        }
        match read_row(&fields, &columns) {
            Ok(rule) => plan.accepted.push(rule),
            Err(reason) => plan.rejected.push(CsvRejection {
                line: line_no,
                row: line.clone(),
                reason,
            }),
        }
    }

    // A circle *within* the file is invisible to the per-row checks, because the first insert
    // would be written before the last row has been read. The plan walks its own rules so a
    // 2,000-line upload is refused before the first write rather than after it.
    if let Some(reason) = refuse_circular_plan(&plan.accepted) {
        plan.rejected.push(CsvRejection {
            line: 0,
            row: String::from("(the file as a whole)"),
            reason,
        });
    }

    Ok(plan)
}

/// Build the CSV text for a site's rules — the same header the importer reads.
///
/// Round-tripping is the point: exporting a site and importing the file into a fresh one
/// reproduces the table, and the walk asserts exactly that rather than eyeballing the columns.
pub fn render_redirect_csv<'a, I>(rows: I) -> String
where
    I: IntoIterator<Item = CsvRedirect>,
{
    let mut out = String::from("from,to,status,pattern,enabled\n");
    for row in rows {
        out.push_str(&quote_field(&row.from_path));
        out.push(',');
        out.push_str(&quote_field(&row.to_path));
        out.push(',');
        out.push_str(&row.status_code.to_string());
        out.push(',');
        out.push_str(row.pattern.as_str());
        out.push(',');
        out.push_str(if row.enabled { "true" } else { "false" });
        out.push('\n');
    }
    out
}

/// Refuse a plan whose own rules send a path in a circle, or into an endless chain.
///
/// Mirrors `SeoStore::refuse_loop`'s walk and its 64-hop cap: the two must agree, or a file the
/// importer accepts becomes a table only one of the two can reason about. `stored` is the rules
/// already in the database, because a file that completes a circle *with* an existing rule is
/// just as much a loop.
pub fn refuse_circular_plan_with(
    stored: &[(String, String)],
    plan: &[CsvRedirect],
) -> Option<String> {
    let mut table: BTreeSet<(String, String)> = stored.iter().cloned().collect();
    for rule in plan {
        table.insert((rule.from_path.clone(), rule.to_path.clone()));
    }

    for rule in plan {
        let mut current = rule.to_path.clone();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        seen.insert(rule.from_path.clone());
        for _ in 0..64 {
            if !seen.insert(current.clone()) {
                return Some(format!(
                    "importing these rules would send a visitor in a circle through '{current}' \
                     (starting at '{}')",
                    rule.from_path
                ));
            }
            let Some((_, next)) = table.iter().find(|(from, _)| from == &current) else {
                break;
            };
            current = next.clone();
        }
        if seen.len() >= 64 {
            return Some(format!(
                "'{}' would join a chain of more than 64 redirects",
                rule.from_path
            ));
        }
    }
    None
}

/// [`refuse_circular_plan_with`] over a plan that has nothing stored yet.
pub fn refuse_circular_plan(plan: &[CsvRedirect]) -> Option<String> {
    refuse_circular_plan_with(&[], plan)
}

// ---------------------------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------------------------

/// Split a CSV into records, honouring quotes and newlines inside quoted fields.
///
/// Records keep their quotes: the comma inside `"/a,b"` must not become a record boundary, and
/// unquoting here would hand `split_fields` a string whose separators are already lost, so it
/// would split a *field* in two. The quoting is undone once, by `split_fields`.
fn split_records(input: &str) -> Vec<String> {
    let mut records = Vec::new();
    let mut current = String::new();
    let mut quoted = false;

    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            // Only a comma or a newline closes a field, so a `"` inside a quoted run is left
            // exactly where it is — including the doubled `""` that spells one quote.
            ',' | '\r' | '\n' if quoted => {
                current.push(ch);
                if (ch == '\r' || ch == '\n') && chars.peek() == Some(&'\n') && ch == '\r' {
                    chars.next();
                    current.push('\n');
                }
            }
            '"' => {
                quoted = !quoted;
                current.push(ch);
            }
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                records.push(std::mem::take(&mut current));
            }
            '\n' => records.push(std::mem::take(&mut current)),
            other => current.push(other),
        }
    }
    if !current.is_empty() {
        records.push(current);
    }
    records
}

/// Split one record into its fields, undoing the quoting.
fn split_fields(record: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = record.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                current.push('"');
            }
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(std::mem::take(&mut current)),
            other => current.push(other),
        }
    }
    fields.push(current);
    fields
}

/// Where each canonical column sits in the file; `None` when the header omits it.
///
/// `from` and `to` are the only required names; the rest default (a redirect is a 301 literal
/// unless the file says otherwise), because the common export from another platform is two
/// columns wide and refusing it would be refusing the file people actually have.
fn map_header(fields: &[String]) -> Result<Vec<Option<usize>>> {
    let mut mapped: Vec<Option<usize>> = vec![None; REDIRECT_CSV_HEADER.len()];
    for (position, name) in fields.iter().enumerate() {
        let normalised = normalise_header(name);
        for (canonical, aliases) in HEADER_ALIASES {
            if aliases.contains(&normalised.as_str()) && mapped[header_index(canonical)].is_none() {
                mapped[header_index(canonical)] = Some(position);
                break;
            }
        }
    }

    for (canonical, slot) in mapped.iter().enumerate() {
        if slot.is_none() && matches!(canonical, 0 | 1) {
            return Err(ContentError::InvalidRedirect(format!(
                "the header needs '{canonical}' — expected one of {}, and it has none",
                REDIRECT_CSV_HEADER.join(", ")
            )));
        }
    }
    Ok(mapped)
}

fn header_index(canonical: &str) -> usize {
    REDIRECT_CSV_HEADER
        .iter()
        .position(|name| *name == canonical)
        .unwrap_or(0)
}

fn required_columns(columns: &[Option<usize>]) -> usize {
    columns.iter().filter(|c| c.is_some()).count()
}

/// Lower-case, trim, and collapse the separators a spreadsheet writes.
fn normalise_header(name: &str) -> String {
    name.trim()
        .trim_start_matches('\u{feff}')
        .to_lowercase()
        .replace(['_', '-'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_blank_or_comment(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.is_empty() || trimmed.starts_with('#')
}

/// Pull one row's five values out of its fields and check them.
fn read_row(
    fields: &[String],
    columns: &[Option<usize>],
) -> std::result::Result<CsvRedirect, String> {
    let get = |index: usize| -> String {
        columns[index]
            .and_then(|position| fields.get(position))
            .map(|value| value.trim().to_string())
            .unwrap_or_default()
    };

    let from_path = get(0);
    if from_path.is_empty() {
        return Err(String::from("'from' is empty"));
    }
    // A row that carries a full URL is the single most common shape of a redirect table, and the
    // store only ever holds site-relative paths, so the origin is stripped rather than refused:
    // the file was not wrong about the destination, it was only more specific than we are.
    let from_path = strip_origin(&from_path).unwrap_or(from_path);
    if from_path.is_empty() {
        return Err(String::from("'from' is empty after removing the origin"));
    }

    let to_path = get(1);
    if to_path.is_empty() {
        return Err(String::from("'to' is empty"));
    }
    // `from` is stripped of its origin and `to` is not, and the asymmetry is the platform's own
    // rule rather than a preference: a rule's `to` has to be a path on THIS site (the store
    // refuses anything else with `the to path must start with '/'`), while a `from` carrying an
    // origin is a table exported from somewhere else and is only being told what it means here.
    // Silently reducing an external destination to its path would send a visitor to
    // `https://this-site/landing` — a page that does not exist — which is worse than a refusal.
    if is_absolute_url(&to_path) {
        return Err(format!(
            "'to' is {to_path:?} — a redirect must point at a path on this site, so remove \
             the origin and check the destination exists here"
        ));
    }

    let status_text = get(2);
    let status_code = if status_text.is_empty() {
        301
    } else {
        // `permanent` and `temporary` are the two words a human writes; the panel's own export
        // writes the number, and both have to load.
        match status_text.to_lowercase().as_str() {
            "permanent" | "moved permanently" => 301,
            "temporary" | "found" | "moved temporarily" => 302,
            other => match other.parse::<i32>() {
                Ok(code) => code,
                Err(_) => {
                    return Err(format!(
                        "'status' is {status_text:?} — use 301, 302, permanent or temporary"
                    ));
                }
            },
        }
    };
    if !crate::seo::REDIRECT_STATUS_CODES.contains(&status_code) {
        return Err(format!("'status' {status_code} is not 301 or 302"));
    }

    let pattern_text = get(3);
    let pattern = if pattern_text.is_empty() {
        CsvPattern::Literal
    } else {
        match pattern_text.to_lowercase().as_str() {
            "literal" | "exact" | "1" => CsvPattern::Literal,
            "regex" | "pattern" | "wildcard" | "2" => CsvPattern::Pattern,
            other => return Err(format!("'pattern' is {other:?} — use literal or regex")),
        }
    };
    if pattern == CsvPattern::Pattern {
        // Validated here rather than at insert so the plan is safe to show before anything is
        // written: the dialect has no backtracking, and a table of them is a request-time cost.
        crate::seo::validate_pattern(&from_path).map_err(|error| error.to_string())?;
    }

    let enabled_text = get(4);
    let enabled = if enabled_text.is_empty() {
        true
    } else {
        match enabled_text.to_lowercase().as_str() {
            "true" | "yes" | "y" | "1" | "on" => true,
            "false" | "no" | "n" | "0" | "off" => false,
            other => return Err(format!("'enabled' is {other:?} — use true or false")),
        }
    };

    Ok(CsvRedirect {
        from_path,
        to_path,
        status_code,
        pattern,
        enabled,
    })
}

/// Drop `https://host` from a `from` value, keeping the path. A table exported from another
/// platform carries full URLs in every column, and the origin is the one part of it that means
/// nothing here — the rule is about a path on this site either way.
fn strip_origin(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.starts_with('/') || trimmed.starts_with('*') {
        return None;
    }
    let (scheme, rest) = trimmed.split_once("://")?;
    if scheme.is_empty() || !rest.contains('/') {
        return None;
    }
    let path = rest.split_once('/')?.1;
    Some(format!("/{path}"))
}

/// `true` for a value that carries its own scheme and host.
fn is_absolute_url(value: &str) -> bool {
    let trimmed = value.trim();
    match trimmed.split_once("://") {
        Some((scheme, rest)) => {
            !scheme.is_empty() && rest.split('/').next().is_some_and(|h| !h.is_empty())
        }
        None => false,
    }
}

/// Quote a field when it holds a comma, a quote or a newline.
fn quote_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn csv(rows: &str) -> RedirectCsv {
        parse_redirect_csv(rows).expect("the file parses")
    }

    #[test]
    fn a_two_column_table_is_the_common_case_and_it_loads() {
        let plan = csv("from,to\n/old,/new\n/older,/newest\n");
        assert_eq!(plan.accepted.len(), 2);
        assert!(
            plan.is_clean(),
            "a two-column file must not be refused: {plan:?}"
        );
        assert_eq!(plan.accepted[0].status_code, 301);
        assert_eq!(plan.accepted[0].pattern, CsvPattern::Literal);
        assert!(plan.accepted[0].enabled);
    }

    #[test]
    fn the_export_round_trips_through_the_importer() {
        let original = vec![
            CsvRedirect {
                from_path: "/a".into(),
                to_path: "/b".into(),
                status_code: 301,
                pattern: CsvPattern::Literal,
                enabled: true,
            },
            CsvRedirect {
                from_path: "/news/*".into(),
                to_path: "/blog/*".into(),
                status_code: 302,
                pattern: CsvPattern::Pattern,
                enabled: false,
            },
        ];
        let plan = csv(&render_redirect_csv(original.clone()));
        assert!(plan.is_clean(), "our own export must load: {plan:?}");
        assert_eq!(plan.accepted, original);
    }

    #[test]
    fn a_quoted_field_may_hold_a_comma_and_a_quote() {
        let plan = csv("from,to,pattern\n\"/a,b\",\"/c\"\"d\",regex\n");
        assert_eq!(plan.accepted[0].from_path, "/a,b");
        assert_eq!(plan.accepted[0].to_path, "/c\"d");
        assert!(csv(&render_redirect_csv(plan.accepted.clone())).is_clean());
    }

    #[test]
    fn a_header_missing_from_or_to_is_refused_with_the_names_it_needs() {
        let error = parse_redirect_csv("source,notes\nold,some text\n").expect_err("no from/to");
        let message = error.to_string();
        assert!(
            message.contains("from"),
            "the refusal must say what is missing: {message}"
        );
        assert!(
            message.contains("to"),
            "the refusal must say what is missing: {message}"
        );
    }

    #[test]
    fn another_platforms_column_names_are_accepted() {
        let plan = csv("source,destination,code,match_type,active\n/old,/new,302,wildcard,no\n");
        assert!(plan.is_clean(), "{plan:?}");
        assert_eq!(plan.accepted[0].status_code, 302);
        assert_eq!(plan.accepted[0].pattern, CsvPattern::Pattern);
        assert!(!plan.accepted[0].enabled);
    }

    #[test]
    fn a_bad_row_is_refused_with_its_line_and_nothing_else_is_guessed() {
        let plan = csv("from,to,status\n/one,/two,301\n/three,,301\n/four,/five,999\n");
        assert_eq!(plan.accepted.len(), 1);
        let lines: Vec<usize> = plan.rejected.iter().map(|r| r.line).collect();
        assert_eq!(
            lines,
            vec![3, 4],
            "line numbers are 1-based and count the header"
        );
        assert!(plan.rejected[0].reason.contains("'to' is empty"));
        assert!(plan.rejected[1].reason.contains("999"));
        assert!(!plan.is_clean());
    }

    #[test]
    fn a_blank_row_is_skipped_and_a_comment_is_skipped() {
        let plan = csv("# exported by something\nfrom,to\n\n/one,/two\n\n# done\n");
        assert_eq!(plan.accepted.len(), 1);
        assert_eq!(plan.skipped_lines, 3);
        assert!(
            plan.is_clean(),
            "blank lines and comments are not refusals: {plan:?}"
        );
    }

    #[test]
    fn a_loop_inside_the_file_is_refused_before_the_first_insert() {
        let plan = csv("from,to\n/a,/b\n/b,/a\n");
        assert!(
            plan.rejected.iter().any(|r| r.reason.contains("circle")),
            "the plan must catch its own circle: {plan:?}"
        );
        assert!(!plan.is_clean());
    }

    #[test]
    fn a_file_that_closes_a_circle_with_a_stored_rule_is_refused_too() {
        let stored = vec![("/b".to_string(), "/a".to_string())];
        let plan = csv("from,to\n/a,/b\n");
        let reason = refuse_circular_plan_with(&stored, &plan.accepted);
        assert!(
            reason.is_some(),
            "the import would complete an existing loop"
        );
    }

    #[test]
    fn a_full_url_from_is_reduced_to_its_path() {
        let plan = csv("from,to\nhttps://old.example.com/old-page,/new\n");
        assert!(plan.is_clean(), "{plan:?}");
        assert_eq!(plan.accepted[0].from_path, "/old-page");
    }

    #[test]
    fn an_external_destination_is_refused_rather_than_reduced_to_its_path() {
        let plan = csv("from,to\n/go,https://partner.example.net/landing\n");
        assert!(
            !plan.is_clean(),
            "the store refuses an external `to`; the plan must too"
        );
        let reason = &plan.rejected[0].reason;
        assert!(
            reason.contains("path on this site"),
            "the refusal must name the rule, not just the row: {reason}"
        );
    }

    #[test]
    fn words_where_a_code_belongs_are_read_as_the_words() {
        let plan = csv("from,to,status\n/a,/b,permanent\n/c,/d,TEMPORARY\n");
        assert_eq!(plan.accepted[0].status_code, 301);
        assert_eq!(plan.accepted[1].status_code, 302);
    }

    #[test]
    fn a_regex_row_is_validated_by_the_platforms_own_dialect() {
        let plan = csv("from,to,pattern\n/a|/b,/c,regex\n");
        assert!(!plan.is_clean(), "an alternation is outside the dialect");
        let plan = csv("from,to,pattern\n/blog/*,/news/*,regex\n");
        assert!(
            plan.is_clean(),
            "the dialect's own shapes must load: {plan:?}"
        );
    }

    #[test]
    fn a_file_over_the_cap_is_refused_rather_than_read() {
        let big = format!(
            "from,to\n{}",
            "/a,/b\n".repeat(MAX_REDIRECT_CSV_BYTES / 6 + 1)
        );
        let error = parse_redirect_csv(&big).expect_err("over the cap");
        assert!(error.to_string().contains("bytes"), "{error}");
    }

    #[test]
    fn an_empty_file_is_refused_with_a_reason() {
        let error = parse_redirect_csv("\n\n").expect_err("nothing to read");
        assert!(error.to_string().contains("header"), "{error}");
    }

    #[test]
    fn the_summary_names_what_happened_in_words() {
        let plan = csv("from,to\n/a,/b\n/c,,301\n");
        let summary = plan.summary();
        assert!(summary.contains("1 rule(s) ready"), "{summary}");
        assert!(summary.contains("1 refused"), "{summary}");
    }

    #[test]
    fn a_header_with_whitespace_and_dashes_is_still_the_header() {
        let plan = csv(" From , To , Status-Code \n/a,/b,302\n");
        assert!(plan.is_clean(), "{plan:?}");
        assert_eq!(plan.accepted[0].status_code, 302);
    }

    #[test]
    fn a_bom_in_front_of_the_first_column_does_not_hide_the_header() {
        let plan = csv("\u{feff}from,to\n/a,/b\n");
        assert!(plan.is_clean(), "{plan:?}");
    }
}
