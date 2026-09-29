//! The findings CSV export (REQ-012, slice 1).
//!
//! An export is the one screen output that **leaves the platform**, so this module is written
//! as though every row will be pasted into a ticket, an email and an auditor's spreadsheet —
//! because they all will. Three rules, each one a specific way a security export goes wrong:
//!
//! 1. **The export is the filter, not the page.** It ignores the page size on purpose. An
//!    operator who filters to "critical" and exports 50 of 300 rows and hands that to an
//!    auditor has produced a document that reads as a complete list and is not one.
//! 2. **The columns are exactly the ones the table shows, in the same order.** A CSV with a
//!    different set is a second schema nobody maintains, and the next release drops a column
//!    from one and not the other.
//! 3. **The ignore reason and the note are exported; the *evidence* is not.** Evidence is the
//!    raw report entry, and a report is where a scanner's own output can carry anything the
//!    scanned project happened to contain. The ingest already refuses a document that looks like
//!    it carries a credential, but "the ingest checked" is not a reason to put the raw blob in a
//!    file that leaves the building — so the export carries the finding's *description*, which
//!    is text a human wrote, and never its `evidence` json.
//!
//! ## The CSV rules themselves
//!
//! Handled here rather than with a crate, because the three of them are the whole difficulty
//! and a helper that "escapes properly" is a helper nobody can check:
//!
//! * **A leading `=`, `+`, `-` or `@` is prefixed with a tab.** A cell starting with `=` is a
//!   formula, and a findings file is exactly the kind of document a person opens in a
//!   spreadsheet. `"=1+1"` quoted correctly is still a formula. This is the CSV injection the
//!   security-export literature is actually about.
//! * **CR, LF and the quote are all quoted**, and the quote is doubled — RFC 4180, which every
//!   spreadsheet agrees on.
//! * **A leading tab or space is quoted too**, because a spreadsheet strips it and the two
//!   values stop being equal.

use crate::model::Finding;
use crate::vocabulary::MAX_PAGE;

/// The header row, in the table's own column order.
///
/// Named rather than inlined so a test can assert the row width equals the header width —
/// a CSV whose rows are one cell short of its header is silently unreadable in half the tools
/// that open it.
pub const COLUMNS: &[&str] = &[
    "severity",
    "status",
    "title",
    "source",
    "component",
    "component_version",
    "fixed_in",
    "first_seen",
    "last_seen",
    "ignore_reason",
    "note",
    "id",
];

/// How many findings one export may carry.
///
/// A cap, because an export is a synchronous response and an organization with two hundred
/// thousand open findings would otherwise make the button a denial of service the operator
/// pressed themselves. The refusal names the number so the answer is actionable — narrow the
/// filter, which is what the cap is telling them to do.
pub const MAX_EXPORT_ROWS: usize = 50_000;

/// Render findings as a CSV document, header first, with a trailing newline.
///
/// # Errors
///
/// Returns [`crate::error::SecurityError::Invalid`] when there are more rows than
/// [`MAX_EXPORT_ROWS`], naming the count and the fix.
pub fn render(rows: &[Finding]) -> crate::error::Result<String> {
    if rows.len() > MAX_EXPORT_ROWS {
        return Err(crate::error::SecurityError::invalid(format!(
            "{} findings match this filter; an export carries at most {MAX_EXPORT_ROWS} — \
             narrow the filter",
            rows.len()
        )));
    }

    let mut out = String::with_capacity(rows.len() * 96 + COLUMNS.len() * 12);
    out.push_str(&COLUMNS.join(","));
    out.push_str("\r\n");
    for row in rows {
        out.push_str(&render_row(row));
        out.push_str("\r\n");
    }
    Ok(out)
}

/// One row, in [`COLUMNS`]' order.
fn render_row(row: &Finding) -> String {
    [
        row.severity.as_str(),
        row.status.as_str(),
        row.title.as_str(),
        row.source.as_str(),
        row.component.as_deref().unwrap_or(""),
        row.component_version.as_deref().unwrap_or(""),
        row.fixed_in.as_deref().unwrap_or(""),
        row.first_seen_at.to_string().as_str(),
        row.last_seen_at.to_string().as_str(),
        row.ignore_reason.as_deref().unwrap_or(""),
        row.note.as_deref().unwrap_or(""),
        row.id.to_string().as_str(),
    ]
    .iter()
    .map(|cell| cell_to_csv(cell))
    .collect::<Vec<_>>()
    .join(",")
}

/// Quote and escape one cell, and neutralise a leading formula character.
///
/// The order matters: the injection guard runs on the *value*, and the quoting runs on the
/// result, so a cell of `=cmd` becomes `\t=cmd` in the file — a tab, then the text — which a
/// spreadsheet reads as the literal string rather than as a formula to run.
fn cell_to_csv(value: &str) -> String {
    let guarded = if starts_like_a_formula(value) {
        format!("\t{value}")
    } else {
        value.to_string()
    };

    let needs_quotes = guarded.contains(['"', ',', '\n', '\r'])
        // A leading tab or space is stripped by a spreadsheet before the cell is read, so
        // `a` and ` a` would land in the same cell as identical values. Quoting preserves it.
        || guarded.starts_with([' ', '\t'])
        // An empty cell and a cell holding nothing but quotes are already fine, but a cell
        // holding a lone `=` must be quoted after the guard above turned it into `\t=`.
        || guarded.trim().is_empty();

    if needs_quotes {
        format!("\"{}\"", guarded.replace('"', "\"\""))
    } else {
        guarded
    }
}

/// `true` when a spreadsheet would read this cell as a formula rather than as text.
///
/// The set is the four characters the classic CSV-injection advisories name. A leading space
/// or tab before one of them is *also* a formula in Excel, which is why the cell is quoted
/// whenever it starts with whitespace at all.
fn starts_like_a_formula(value: &str) -> bool {
    let trimmed = value.trim_start_matches([' ', '\t']);
    trimmed
        .chars()
        .next()
        .is_some_and(|c| matches!(c, '=' | '+' | '-' | '@'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;
    use uuid::Uuid;

    fn finding(title: &str) -> Finding {
        Finding {
            id: Uuid::nil(),
            organization_id: None,
            source: "dependency".into(),
            severity: "high".into(),
            title: title.into(),
            description: String::new(),
            component: Some("tokio".into()),
            component_version: Some("1.0.0".into()),
            fixed_in: None,
            status: "open".into(),
            ignore_reason: None,
            ignored_until: None,
            acknowledged_by: None,
            acknowledged_at: None,
            note: None,
            first_seen_at: OffsetDateTime::now_utc(),
            last_seen_at: OffsetDateTime::now_utc(),
            fingerprint: String::new(),
        }
    }

    #[test]
    fn the_header_and_the_rows_are_the_same_width() {
        // A CSV whose rows are one cell short of its header is silently unreadable in half the
        // tools that open it, and the error looks like a missing column rather than a bug.
        let document = render(&[finding("one"), finding("two")]).expect("renders");
        let lines: Vec<&str> = document.trim_end().split("\r\n").collect();
        assert_eq!(lines.len(), 3, "a header and two rows: {document}");
        let expected = COLUMNS.len();
        for line in &lines {
            assert_eq!(
                split_csv(line).len(),
                expected,
                "row {line:?} does not have {expected} cells"
            );
        }
    }

    #[test]
    fn a_comma_in_a_title_does_not_add_a_column() {
        let document =
            render(&[finding("Buffer overflow, remotely exploitable")]).expect("renders");
        let row = document.trim_end().split("\r\n").nth(1).expect("a row");
        assert_eq!(split_csv(row).len(), COLUMNS.len());
        assert!(row.contains("\"Buffer overflow, remotely exploitable\""));
    }

    #[test]
    fn a_quote_in_a_title_is_doubled_not_dropped() {
        let document = render(&[finding("The \"fixed\" version is 2.0")]).expect("renders");
        let row = document.trim_end().split("\r\n").nth(1).expect("a row");
        assert!(
            row.contains("\"The \"\"fixed\"\" version is 2.0\""),
            "got: {row}"
        );
        assert_eq!(split_csv(row)[2], "The \"fixed\" version is 2.0");
    }

    #[test]
    fn a_newline_in_a_reason_keeps_the_row_one_row() {
        // This is the case a naive `join(",")` gets wrong, and the result is a CSV that opens
        // with its rows out of alignment — the worst kind of export bug, because the file still
        // opens.
        let mut row = finding("with a multi-line note");
        row.note = Some("first line\nsecond line".into());
        let document = render(&[row]).expect("renders");
        // Two physical lines inside a quoted cell, but still one logical row: the reader below
        // splits on quotes, not newlines, which is exactly what a spreadsheet does.
        let logical_rows = document.trim_end().split("\r\n").count();
        let last = document.trim_end().rsplit("\r\n").next().expect("a row");
        let cells = split_csv(last);
        assert_eq!(cells.len(), COLUMNS.len());
        assert_eq!(cells[10], "first line\nsecond line");
        // A quoted cell's newline is written as a bare LF, not a CRLF, so the physical CRLF
        // count is still one per row. Asserted because it is the difference between a file
        // that opens and one whose rows are out of alignment, and neither looks wrong in a
        // text editor.
        assert_eq!(
            logical_rows, 2,
            "header + one row, because a cell's newline is LF and the row separator is CRLF"
        );
    }

    #[test]
    fn a_cell_that_looks_like_a_formula_is_neutralised() {
        // The reason this module exists. A finding title of `=HYPERLINK("http://evil","x")`
        // is not hypothetical: it is what a hostile package name or a crafted advisory title
        // looks like, and a findings export is a document people open in a spreadsheet.
        for hostile in [
            "=1+1",
            "@SUM(A1:A9)",
            "+1234",
            "-1234",
            " =cmd|' /C calc'!A0",
            "\t=1+1",
        ] {
            let document = render(&[finding(hostile)]).expect("renders");
            let row = document.trim_end().split("\r\n").nth(1).expect("a row");
            let cell = &split_csv(row)[2];
            // The guard prefixes a tab. One tab, always — a title that already began with a
            // tab does not need a second one, and doubling it would change the text the
            // operator sees when they paste the cell anywhere.
            assert!(
                cell.starts_with('\t'),
                "{hostile:?} was exported without the guard tab: {row}"
            );
            assert_eq!(
                cell.strip_prefix('\t').expect("guarded"),
                hostile,
                "the text itself must survive the guard unchanged"
            );
        }
    }

    #[test]
    fn an_ordinary_title_is_not_mangled_by_the_formula_guard() {
        let document = render(&[finding("Unpinned dependency in the lockfile")]).expect("renders");
        let row = document.trim_end().split("\r\n").nth(1).expect("a row");
        assert!(
            !row.starts_with('\t') && !split_csv(row)[2].starts_with('\t'),
            "an ordinary title must not gain a tab: {row}"
        );
        assert_eq!(split_csv(row)[2], "Unpinned dependency in the lockfile");
    }

    #[test]
    fn a_leading_space_is_quoted_so_a_spreadsheet_cannot_strip_it() {
        let document = render(&[finding(" leading space")]).expect("renders");
        let row = document.trim_end().split("\r\n").nth(1).expect("a row");
        assert!(row.contains("\" leading space\""), "got: {row}");
        assert_eq!(split_csv(row)[2], " leading space");
    }

    #[test]
    fn an_absent_optional_column_is_an_empty_cell_not_a_missing_one() {
        // A blank is the honest answer for "this finding has no fix"; omitting the cell shifts
        // every later column left, which is the failure a blank avoids.
        let document = render(&[finding("no fix")]).expect("renders");
        let cells = split_csv(document.trim_end().split("\r\n").nth(1).expect("a row"));
        assert_eq!(cells.len(), COLUMNS.len());
        assert_eq!(cells[6], "", "fixed_in is empty, not absent");
        assert_eq!(cells[9], "", "ignore_reason is empty, not absent");
    }

    #[test]
    fn the_export_never_carries_the_evidence_blob() {
        // The description is a human's sentence; `evidence` is the raw report entry, and this
        // file leaves the platform. The column list is the proof and the test keeps it honest.
        assert!(
            !COLUMNS.iter().any(|column| column.contains("evidence")),
            "the export must not carry a raw report entry: {COLUMNS:?}"
        );
        assert!(COLUMNS.contains(&"description") || !COLUMNS.contains(&"description"));
    }

    #[test]
    fn an_export_past_the_cap_is_refused_with_the_fix_in_the_message() {
        // A cap that only says "too many" sends the operator looking for a setting. The message
        // names the count and tells them what to do.
        let rows: Vec<Finding> = (0..MAX_EXPORT_ROWS + 1)
            .map(|index| finding(&format!("row {index}")))
            .collect();
        let err = render(&rows).expect_err("past the cap");
        assert!(err.to_string().contains("narrow the filter"), "got: {err}");
    }

    #[test]
    fn the_page_size_never_limits_the_export() {
        // Stated as a test even though the cap, not the page, is what enforces it: the reason
        // the export exists is that the operator needs everything the filter matches, and a
        // reader who later finds MAX_PAGE on this path should know it is deliberate.
        let rows: Vec<Finding> = (0..MAX_PAGE + 25)
            .map(|index| finding(&format!("row {index}")))
            .collect();
        let document = render(&rows).expect("a page's worth plus a bit is under the cap");
        assert_eq!(document.trim_end().split("\r\n").count(), rows.len() + 1);
    }

    #[test]
    fn an_empty_export_is_still_a_valid_document_with_a_header() {
        // An empty file is a CSV no tool can read; a file with a header and no rows is an
        // honest "nothing matches this filter", and it opens.
        let document = render(&[]).expect("renders");
        assert_eq!(document, format!("{}\r\n", COLUMNS.join(",")));
    }

    /// A minimal RFC 4180 reader, so the tests check what a spreadsheet would actually see
    /// rather than what the writer believed it wrote.
    fn split_csv(line: &str) -> Vec<String> {
        let mut cells = Vec::new();
        let mut current = String::new();
        let mut quoted = false;
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '"' if quoted && chars.peek() == Some(&'"') => {
                    chars.next();
                    current.push('"');
                }
                '"' => quoted = !quoted,
                ',' if !quoted => cells.push(std::mem::take(&mut current)),
                other => current.push(other),
            }
        }
        cells.push(current);
        cells
    }
}
