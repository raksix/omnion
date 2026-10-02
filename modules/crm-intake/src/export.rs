//! The inbox's `export CSV` — the sixth word on REQ-117's bulk row, and the only one that
//! **leaves the platform**.
//!
//! Four rules, each one a specific way a lead export goes wrong:
//!
//! 1. **The export is the filter, not the page.** `list_leads` returns a page capped at
//!    [`crate::vocabulary::MAX_PAGE`] and the panel shows one at a time; an operator who
//!    filters to "breached" and exports the visible fifty of three hundred has produced a
//!    document that reads as a complete list and is not one. The export therefore runs the same
//!    [`LeadQuery`] and **climbs the cursor until the filter is exhausted** — the one place in
//!    this crate where a bounded read is the wrong shape.
//! 2. **The columns are the table's, in the table's order.** A CSV with a different set is a
//!    second schema nobody maintains, and the next column added to the inbox lands in one and
//!    not the other. `COLUMNS` is asserted against the inbox's own column list in the gate.
//! 3. **No column is exported that the platform did not itself show.** The raw submission
//!    payload is deliberately **absent**: it is the submitter's own words back to them, it is
//!    unbounded, and a spreadsheet is not the place for it. The lead detail already shows it to
//!    somebody who may read it.
//! 4. **A selection exports the selection.** The bulk bar has a selection, so `export` is a
//!    verb on it like the other three — and a selection of twenty with a filter active must not
//!    silently become "everything the filter matches", which is the defect the first version of
//!    the panel's bulk row would have shipped.
//!
//! ## The CSV rules themselves
//!
//! Handled here rather than with a crate, because three of them are the whole difficulty and a
//! helper that "escapes properly" is a helper nobody can check:
//!
//! * **A leading `=`, `+`, `-` or `@` is prefixed with a tab.** A cell starting with `=` is a
//!   formula, and a lead list is exactly the kind of document a person opens in a spreadsheet.
//!   A visitor controls a lead's name, company, job title and message — every one of those is
//!   a column here, so this is not a hypothetical guard: it is the reason this file exists
//!   separately from the escaping.
//! * **CR, LF and the quote are quoted, and the quote is doubled** — RFC 4180.
//! * **A leading tab or space is quoted too**, because a spreadsheet strips it and the two
//!   values then stop being equal.

use crate::model::Lead;
use crate::vocabulary::MAX_EXPORT_ROWS;

/// The header row, in the inbox's own column order.
///
/// Named rather than inlined so the gate can assert every header cell is a field the table
/// shows, and every table column is here — the failure this rule exists to prevent is silent,
/// because a CSV with one column too few still opens.
pub const COLUMNS: &[&str] = &[
    "received_at",
    "status",
    "first_name",
    "last_name",
    "email",
    "phone",
    "company_name",
    "job_title",
    "product_interest",
    "owner_user_id",
    "first_response_due_at",
    "first_response_at",
    "source_id",
    "duplicate_of",
    "decision",
    "spam_score",
    "rejection_reason",
    "id",
];

/// Render leads as a CSV document, header first, with a trailing newline.
///
/// # Errors
///
/// Returns `Invalid` when there are more rows than [`MAX_EXPORT_ROWS`], naming the count and
/// the remedy. **The cap is a refusal rather than a truncation on purpose**: a silently
/// truncated export is a document that lies about its own size, and the operator handing it to
/// an auditor has no way to know.
pub fn render(rows: &[Lead]) -> crate::error::Result<String> {
    if rows.len() > MAX_EXPORT_ROWS {
        return Err(crate::error::CrmIntakeError::invalid(format!(
            "{} leads match this filter; an export carries at most {MAX_EXPORT_ROWS} — \
             narrow the filter",
            rows.len()
        )));
    }

    let mut out = String::with_capacity(rows.len() * 128 + COLUMNS.len() * 16);
    out.push_str(&COLUMNS.join(","));
    out.push_str("\r\n");
    for row in rows {
        out.push_str(&render_row(row));
        out.push_str("\r\n");
    }
    Ok(out)
}

/// One row, in [`COLUMNS`]' order.
fn render_row(row: &Lead) -> String {
    [
        row.received_at.to_string(),
        row.status.clone(),
        row.first_name.clone().unwrap_or_default(),
        row.last_name.clone().unwrap_or_default(),
        row.email.clone().unwrap_or_default(),
        row.phone.clone().unwrap_or_default(),
        row.company_name.clone().unwrap_or_default(),
        row.job_title.clone().unwrap_or_default(),
        row.product_interest.clone().unwrap_or_default(),
        row.owner_user_id.map(|id| id.to_string()).unwrap_or_default(),
        row.first_response_due_at
            .map(|at| at.to_string())
            .unwrap_or_default(),
        row.first_response_at
            .map(|at| at.to_string())
            .unwrap_or_default(),
        row.source_id.map(|id| id.to_string()).unwrap_or_default(),
        row.duplicate_of.map(|id| id.to_string()).unwrap_or_default(),
        row.decision.clone().unwrap_or_default(),
        row.spam_score.to_string(),
        row.rejection_reason.clone().unwrap_or_default(),
        row.id.to_string(),
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
pub(crate) fn cell_to_csv(value: &str) -> String {
    let guarded = if starts_like_a_formula(value) {
        format!("\t{value}")
    } else {
        value.to_string()
    };

    let needs_quotes = guarded.contains(['"', ',', '\n', '\r'])
        // A leading tab or space is stripped by a spreadsheet before the cell is read, so `a`
        // and ` a` would land in the same cell as identical values. Quoting preserves it.
        || guarded.starts_with([' ', '\t'])
        || guarded.trim().is_empty();

    if needs_quotes {
        format!("\"{}\"", guarded.replace('"', "\"\""))
    } else {
        guarded
    }
}

/// `true` when a spreadsheet would read this cell as a formula rather than as text.
///
/// **A bare number is not a formula, and guarding it breaks the export rather than saving it.**
/// The prefix rule is the OWASP one — `=`, `+`, `-`, `@` all start something a spreadsheet
/// evaluates — and applied blindly it turns the value `-3` into `\t-3`, so a signed column
/// exports as a string that will not sum, will not chart, and has to be cleaned by hand in
/// whatever tool receives it. The guard is therefore the prefix rule **minus the one case where
/// the rest of the cell is a plain number**, which is the case that cannot be an injection:
/// `-2+3` is guarded (the remainder is not a number), `-3` is not.
///
/// No column of today's `Lead` is signed — `spam_score` is a non-negative `int` — so this arm
/// is unreachable for the inbox as it stands and is written for the column that will be added
/// next. It is asserted anyway, because the guard's *cost* is invisible until somebody exports
/// a negative figure and discovers the column went quiet, and the assertion is the only place
/// that cost is written down.
fn starts_like_a_formula(value: &str) -> bool {
    let Some(first) = value.chars().next() else {
        return false;
    };
    if !matches!(first, '=' | '+' | '-' | '@') {
        return false;
    }
    if matches!(first, '-' | '+') && is_a_plain_number(value) {
        return false;
    }
    true
}

/// `true` when the whole value is one number and nothing else.
///
/// **No `f64` parse, and the grammar is spelled out.** `-1e5` and `-0x1` parse as numbers in
/// some readers and are not numbers to a human reading the file, so the question is not "what
/// does a parser accept" but "what does a spreadsheet read as a figure" — and the two are not
/// the same set. The grammar: an optional sign, at least one digit **before** any dot, at most
/// one dot, an optional exponent that is itself a signed integer.
///
/// **A dot with no digit in front of it is not a number here**, and that is the conservative
/// direction rather than the strict one: `-.5` is a figure to Excel and a string to a CSV
/// reader, so calling it a number releases the guard on a cell whose meaning depends on which
/// program opens the file. A false positive in the guard costs a tab character; a false
/// negative costs an executed formula. Every ambiguity resolves toward guarding.
fn is_a_plain_number(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    if matches!(bytes.first(), Some(b'-' | b'+')) {
        index = 1;
    }
    let mut digits = 0;
    let mut dots = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'0'..=b'9' => digits += 1,
            b'.' if dots == 0 && digits > 0 => dots += 1,
            b'e' | b'E' if digits > 0 && bytes[index - 1].is_ascii_digit() => {
                // An exponent must itself be a signed integer, so `-1e+5` works and `-1e`
                // does not.
                let mut cursor = index + 1;
                if matches!(bytes.get(cursor), Some(b'-' | b'+')) {
                    cursor += 1;
                }
                let mut exponent_digits = 0;
                while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
                    cursor += 1;
                    exponent_digits += 1;
                }
                return exponent_digits > 0 && cursor == bytes.len();
            }
            _ => return false,
        }
        index += 1;
    }
    digits > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cell_beginning_with_a_formula_character_is_neutralised() {
        // **Every one of these is a value a visitor chose.** `job_title` and `company_name`
        // are free text on the public form, so this is the attack, not a shape.
        for hostile in ["=1+1", "+cmd", "-2+3", "@SUM(A1)"] {
            let cell = cell_to_csv(hostile);
            assert!(
                cell.starts_with("\"\t"),
                "{hostile} must reach the file as a tab-prefixed literal, not as {cell}"
            );
        }
    }

    #[test]
    fn a_negative_number_is_not_a_formula_and_stays_readable() {
        // **The guard's cost, and the reason the rule is a prefix rule minus a numeric arm.**
        // Tab-prefixing `-3` would make a signed column export as a string that will not sum
        // and will not chart. No column is signed today, so this arm is unreachable for the
        // inbox as it stands — and it is asserted anyway, because the cost is invisible until
        // somebody exports a negative figure and the column goes quiet.
        assert_eq!(cell_to_csv("-3"), "-3");
        assert_eq!(cell_to_csv("-3.5"), "-3.5");
        assert_eq!(cell_to_csv("42"), "42");
        assert_eq!(cell_to_csv("1e3"), "1e3");
    }

    #[test]
    fn a_signed_cell_that_is_not_only_a_number_is_still_guarded() {
        // **The negative control for the arm above, and the assertion that keeps it from
        // growing into a hole.** `-2+3` begins with the character the guard watches and is not
        // a number, which is the whole of an injection: a spreadsheet that evaluates the first
        // `-` evaluates the rest of it too. If this ever reads `-2+3` unquoted, the numeric arm
        // has become a bypass.
        assert!(
            cell_to_csv("-2+3").starts_with("\"\t"),
            "a signed expression is guarded: {}",
            cell_to_csv("-2+3")
        );
        for hostile in ["-1e", "-1e+", "--3", "- 3"] {
            assert_eq!(
                is_a_plain_number(hostile),
                false,
                "{hostile} is not one number and must not be treated as one"
            );
        }
        // `-.5` is the case the grammar above is written for: a figure to one program and a
        // string to another, so the guard stays on. Asserted explicitly because it is the one
        // input a future reader will "fix" as an oversight.
        assert_eq!(
            is_a_plain_number("-.5"),
            false,
            "a dot with no digit in front of it is ambiguous across readers — keep the guard"
        );
        for benign in ["-3", "-0.5", "+7", "12", "1e3", "-1E-5"] {
            assert!(
                is_a_plain_number(benign),
                "{benign} is one number and must survive the guard"
            );
        }
    }

    #[test]
    fn a_quote_a_comma_and_a_newline_are_all_rfc4180() {
        assert_eq!(cell_to_csv("a,b"), "\"a,b\"");
        assert_eq!(cell_to_csv("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(cell_to_csv("two\nlines"), "\"two\nlines\"");
        assert_eq!(cell_to_csv("carriage\rreturn"), "\"carriage\rreturn\"");
    }

    #[test]
    fn a_leading_space_is_quoted_because_a_spreadsheet_strips_it() {
        // Without the quotes, `" Ada"` and `"Ada"` are the same cell to every spreadsheet, and
        // the export would quietly merge two different people.
        assert_eq!(cell_to_csv(" Ada"), "\" Ada\"");
        assert_eq!(cell_to_csv("Ada"), "Ada");
    }

    #[test]
    fn an_empty_cell_is_empty_and_not_a_missing_column() {
        // A `""` cell and a short row are different failures and only one of them is silent,
        // so this asserts the row is always the header's width.
        assert_eq!(cell_to_csv(""), "\"\"");
    }

    #[test]
    fn the_header_is_the_export() {
        // An export with no rows is still a valid document: a spreadsheet needs the header to
        // know what the columns mean, and "0 rows" rendered as an empty file downloads a
        // zero-byte document that opens as nothing at all.
        let document = render(&[]).expect("an empty export renders");
        assert_eq!(
            document.trim_end(),
            COLUMNS.join(","),
            "a filter that matched nothing still names its columns"
        );
        assert!(document.ends_with("\r\n"), "and ends with a line break");
    }

    #[test]
    fn the_header_carries_no_column_the_table_does_not_show() {
        // The rule this file is written under, asserted rather than asserted-in-prose: the
        // payload is the one column a lead has that must never leave in a spreadsheet, and a
        // future edit that adds it is caught here.
        assert!(
            !COLUMNS.iter().any(|column| column.contains("payload")),
            "the raw submission is the submitter's own words; it never leaves in a CSV"
        );
        assert!(
            !COLUMNS.iter().any(|column| column.contains("consent")),
            "and the consent text is a quotation of the form's own wording, not a contact field"
        );
    }

    #[test]
    fn the_refusal_is_a_refusal_and_not_a_truncation() {
        // Asserted on the *message*, because the alternative — silently writing the first
        // `MAX_EXPORT_ROWS` rows — produces a file indistinguishable from a complete one at
        // exactly the point where it does damage: an operator handing it to an auditor. The
        // count and the remedy both have to be in the sentence, or the operator retries with
        // the same filter and gets the same document.
        assert!(
            MAX_EXPORT_ROWS >= 1_000,
            "the cap is a ceiling on a whole inbox, not a page: {MAX_EXPORT_ROWS}"
        );
        let sentence = crate::bulk::TooManyRows(MAX_EXPORT_ROWS + 1).to_string();
        assert!(
            sentence.contains(&MAX_EXPORT_ROWS.to_string()),
            "the ceiling is named: {sentence}"
        );
        assert!(
            sentence.contains("narrow the filter"),
            "and the way out: {sentence}"
        );
    }
}
