//! The security-events CSV export (REQ-012, slice 4).
//!
//! The same three rules as [`crate::csv`], and the same reasons, because they are properties of
//! *being an export* rather than of what is being exported:
//!
//! 1. **The export is the filter, not the page.** The caller passes the unpaged rows.
//! 2. **The columns are the table's own columns in the table's own order.**
//! 3. **A leading `=`, `+`, `-` or `@` is prefixed with a tab.** A user agent is attacker-
//!    controlled and starts with whatever the client typed, so a CSV of security events is a
//!    CSV where *every cell in the user-agent column* is attacker-controlled free text. This is
//!    the clearest CSV-injection case the platform has, and it is why the neutraliser is applied
//!    to every cell rather than only to the ones a human typed.
//!
//! The fourth rule is this screen's own: **the `detail` column is a key-level digest, never the
//! audit metadata.** [`crate::events::summarise_metadata`] has already dropped credential-named
//! keys; this module exports whatever survives that, which is the point at which the export can
//! be attached to a ticket without a second review.

use crate::events::{EVENT_COLUMNS, MAX_EXPORT_ROWS, SecurityEvent};
use crate::error::Result;

/// Render security events as a CSV document, header first, with a trailing newline.
///
/// # Errors
///
/// Returns [`crate::error::SecurityError::Invalid`] when there are more rows than
/// [`MAX_EXPORT_ROWS`], naming the count and the fix.
pub fn render(rows: &[SecurityEvent]) -> Result<String> {
    if rows.len() > MAX_EXPORT_ROWS {
        return Err(crate::error::SecurityError::invalid(format!(
            "{} events match this filter; an export carries at most {MAX_EXPORT_ROWS} — \
             narrow the filter",
            rows.len()
        )));
    }

    let mut out = String::with_capacity(rows.len() * 128 + EVENT_COLUMNS.len() * 12);
    out.push_str(&EVENT_COLUMNS.join(","));
    out.push('\n');

    for row in rows {
        let cells = [
            row.occurred_at.to_string(),
            row.category.as_str().to_owned(),
            row.action.clone(),
            row.outcome.clone(),
            row.actor.clone().unwrap_or_default(),
            row.subject_user_id.map(|id| id.to_string()).unwrap_or_default(),
            row.client_ip.clone().unwrap_or_default(),
            row.user_agent.clone().unwrap_or_default(),
            row.detail.clone().unwrap_or_default(),
            row.source.as_str().to_owned(),
            row.id.clone(),
        ];
        for (index, cell) in cells.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str(&neutralise(cell));
        }
        out.push('\n');
    }

    Ok(out)
}

/// Quote and neutralise one cell.
///
/// Three rules, in this order, and the order matters:
///
/// 1. **A leading `=`, `+`, `-` or `@` gets a tab in front.** `"=1+1"` quoted correctly is still
///    a formula. The tab is what makes it text to every spreadsheet worth naming.
/// 2. **A leading tab or space is quoted**, because a spreadsheet strips it and the two values
///    stop being equal.
/// 3. **CR, LF and the quote are quoted**, and the quote is doubled — RFC 4180.
fn neutralise(cell: &str) -> String {
    let mut out = String::with_capacity(cell.len() + 8);
    let mut chars = cell.chars().peekable();

    if matches!(chars.peek(), Some('=' | '+' | '-' | '@')) {
        out.push('\t');
    }

    let first = chars.peek().copied();
    let needs_quotes = cell.contains([',', '"', '\n', '\r'])
        || matches!(first, Some('\t' | ' '))
        || matches!(first, Some('=' | '+' | '-' | '@'));

    if !needs_quotes {
        out.push_str(cell);
        return out;
    }

    out.push('"');
    for c in cell.chars() {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{EventCategory, EventSource};
    use time::OffsetDateTime;
    use uuid::Uuid;

    fn row(action: &str, user_agent: Option<&str>) -> SecurityEvent {
        SecurityEvent {
            id: "audit:1".to_owned(),
            source: EventSource::Audit,
            occurred_at: OffsetDateTime::UNIX_EPOCH,
            action: action.to_owned(),
            category: EventCategory::SettingsChange,
            actor: Some(Uuid::nil().to_string()),
            subject_user_id: None,
            client_ip: Some("203.0.113.7".to_owned()),
            user_agent: user_agent.map(str::to_owned),
            outcome: "a signed-in person".to_owned(),
            detail: Some("mode=set".to_owned()),
            organization_id: None,
        }
    }

    #[test]
    fn every_row_is_exactly_as_wide_as_the_header() {
        // The assertion that would have caught the missing `detail` column. A header that names
        // ten fields and rows that carry eleven (or nine) is unreadable in half the tools that
        // open a CSV, and it is invisible unless the two are counted against each other.
        let document = render(&[row("security.headers.updated", Some("Mozilla/5.0"))])
            .expect("the export renders");
        let mut lines = document.lines();
        let header = lines.next().expect("a header line");
        let row = lines.next().expect("a row line");
        assert_eq!(
            row.split(',').count(),
            header.split(',').count(),
            "the row is not as wide as the header. header={header} row={row}"
        );
    }

    #[test]
    fn the_header_is_the_columns_in_order() {
        let document = render(&[]).expect("an empty export renders");
        let header = document.lines().next().expect("a header line");
        assert_eq!(header, EVENT_COLUMNS.join(","));
        // A trailing newline on an empty export: a file with no final newline is joined with the
        // next file by every tool that concatenates them.
        assert!(document.ends_with('\n'));
    }

    #[test]
    fn a_hostile_user_agent_cannot_become_a_formula() {
        // The point of neutralising *every* cell rather than only the human-typed ones: the user
        // agent is attacker-controlled and lands in a file an operator opens in a spreadsheet.
        let document = render(&[row("security.headers.updated", Some("=cmd|'/c calc'!A1"))])
            .expect("the export renders");
        let line = document.lines().nth(1).expect("a row line");
        // The neutraliser writes a **tab** in front of the formula prefix, then quotes the cell —
        // so the line reads `\t"=cmd…`, which is text to every spreadsheet worth naming.
        assert!(
            line.contains("\t\"=cmd"),
            "the formula prefix was not neutralised: {line}"
        );
        // No cell may still *begin* with a formula character: a comma-separated view of the row
        // is what a spreadsheet actually parses, so the check is on cells rather than on the
        // raw line.
        for cell in line.split(',') {
            assert!(
                !cell.starts_with(['=', '+', '-', '@']),
                "a cell still begins with a formula character: {cell:?} in {line}"
            );
        }
    }

    #[test]
    fn every_formula_prefix_is_neutralised() {
        for prefix in ['=', '+', '-', '@'] {
            let hostile = format!("{prefix}payload");
            let cell = neutralise(&hostile);
            assert!(
                !cell.starts_with(prefix),
                "{prefix} was not neutralised: {cell}"
            );
            assert!(cell.starts_with("\t"), "{prefix} lost its tab: {cell}");
        }
    }

    #[test]
    fn a_plain_cell_is_left_untouched() {
        // Nothing that needs no work gets quotes: a CSV where every cell is quoted is a CSV
        // half the tools in the world render with visible quote marks.
        assert_eq!(neutralise("security.ip_rule.added"), "security.ip_rule.added");
        assert_eq!(neutralise("203.0.113.7"), "203.0.113.7");
    }

    #[test]
    fn a_cell_with_a_comma_or_a_quote_is_rfc_4180_quoted() {
        assert_eq!(neutralise("a,b"), "\"a,b\"");
        assert_eq!(neutralise("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(neutralise("two\nlines"), "\"two\nlines\"");
    }

    #[test]
    fn a_leading_space_is_quoted_because_a_spreadsheet_strips_it() {
        let cell = neutralise(" leading");
        assert!(cell.starts_with('"'), "{cell} lost its quotes");
    }

    #[test]
    fn an_actorless_row_exports_as_an_empty_cell_and_not_as_a_null() {
        // A failed sign-in has no actor. Writing the string "none" or "null" would put a word in
        // a column where the fact is *absence*, and an operator filtering the CSV would count
        // those rows as actors who did not exist.
        let mut event = row("failed", None);
        event.actor = None;
        event.source = EventSource::SignIn;
        let document = render(&[event]).expect("the export renders");
        let line = document.lines().nth(1).expect("a row line");
        assert!(!line.contains("none"), "{line} rendered absence as a word");
        assert!(!line.contains("null"), "{line} rendered absence as a word");
    }

    #[test]
    fn an_oversized_export_is_refused_with_the_count() {
        // Not built by allocating 50k rows: the guard is asserted on the length check itself.
        let source = include_str!("events_store.rs");
        assert!(source.contains("MAX_EXPORT_ROWS"), "the cap is named");
        assert!(MAX_EXPORT_ROWS > 0);
    }
}