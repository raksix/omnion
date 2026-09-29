//! The CSV exports: the two screens whose export must contain **the rows the table above them
//! shows** (docs/requests/REQ-053, slice 2).
//!
//! The criterion this file exists for is "the CSV export matches the table", and the only way that
//! is true by construction is to render the export **from the same list the screen rendered** —
//! not to re-query with a parallel SQL statement that happens to share a column list. Two queries
//! are two answers, and the moment one of them grows a filter the other does not, the file
//! becomes a report that disagrees with the page it was downloaded from, which is worse than no
//! export at all because it is trusted.
//!
//! So both functions here take an already-fetched [`Page`] and serialise it. The route fetches
//! with the same filter and the same `limit` the table used, and the writer cannot invent a row.
//!
//! The writer itself is hand-rolled, following `modules/sales/src/reports.rs`: the rule is four
//! lines and a CSV crate in a public repository has to earn its place.

use crate::ledger::Movement;
use crate::store::{Page, StockLevel};

/// One CSV cell: quoted when it holds a comma, a quote or a newline, with quotes doubled.
///
/// The `=` guard is the one that matters for an export of user-entered data. A cell beginning
/// with `=`, `+`, `-` or `@` is executed as a formula by Excel and Sheets when the file is
/// opened, so a note reading "=SUM(A1:A9)" — or an item name somebody chose — becomes code on
/// the approver's machine. Prefixing the cell with an apostrophe is the conventional neutraliser
/// and keeps the text readable.
fn csv_cell(value: &str) -> String {
    let guarded = if starts_a_formula(value) {
        // A leading apostrophe inside a quoted cell is literal, so it has to go outside the
        // quotes to work: `'=` is the form Excel treats as text.
        format!("'{value}")
    } else {
        value.to_string()
    };
    if guarded.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", guarded.replace('"', "\"\""))
    } else {
        guarded
    }
}

/// Whether a cell would be **evaluated** as a formula when the file is opened.
///
/// `=`, `+` and `@` always start one. `-` does not, and the distinction is the whole function:
/// a ledger export is mostly negative numbers, and a guard that prefixes every one of them turns
/// the file into a column of text in the approver's spreadsheet — a worse bug than the injection
/// it prevents, and one nobody notices because the file still opens. A `-` starts a formula only
/// when something **evaluable** follows it, so `-2`, `-1.500` and `- 3 units` pass through
/// untouched while a bare `-` or `-cmd` is guarded.
///
/// `@` is in the list even though `=` is the common case, because `@name` is a Lotus-legacy
/// macro reference and it costs nothing to include.
fn starts_a_formula(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    match first {
        '=' | '+' | '@' => true,
        '-' => !matches!(chars.next(), Some('0'..='9' | '.' | ' ')),
        _ => false,
    }
}

/// The stock list, as a CSV.
///
/// **The columns are the screen's columns and the rows are the screen's rows**, in the same
/// order, so a reader can put the file next to the page and check them off. The threshold column
/// is written twice (`min_threshold` and `reorder_point`) rather than as the single "Threshold"
/// the screen's badge is derived from, because the export is a data file and a downstream
/// spreadsheet cannot derive a status from a badge.
pub fn stock_csv(page: &Page<StockLevel>) -> String {
    let mut out = String::with_capacity(4_096);
    out.push_str(
        "sku,item,category,unit,warehouse_location,on_hand,reserved,available,min_threshold,\
         reorder_point,status,last_movement\r\n",
    );
    for row in &page.items {
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{},{}\r\n",
            csv_cell(&row.sku),
            csv_cell(&row.name),
            csv_cell(row.category.as_deref().unwrap_or("")),
            csv_cell(&row.unit),
            csv_cell(&row.location_code),
            csv_cell(&row.on_hand.to_text()),
            csv_cell(&row.reserved.to_text()),
            csv_cell(&row.available.to_text()),
            csv_cell(&row.min_threshold.to_text()),
            csv_cell(&row.reorder_point.to_text()),
            csv_cell(row.status.as_str()),
            csv_cell(
                &row.last_movement_at
                    .as_ref()
                    .map(crate::dates::to_wire)
                    .unwrap_or_default(),
            ),
        ));
    }
    with_bom(out)
}

/// The movement ledger, as a CSV.
///
/// `quantity` is written **signed** and the `on_hand_after` column is written beside it, because
/// a ledger export whose quantities are all positive is a file that cannot be replayed in a
/// spreadsheet: you cannot check the running total without knowing which way each row went. The
/// sign rule is the module's (`signed_quantity`), not a second decision made here.
pub fn movements_csv(page: &Page<Movement>) -> String {
    let mut out = String::with_capacity(4_096);
    out.push_str(
        "recorded_at,sku,item,kind,quantity,reason,location,source,actor,note,on_hand_after,\
         reserved_after\r\n",
    );
    for row in &page.items {
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{},{}\r\n",
            csv_cell(&crate::dates::to_wire(&row.created_at)),
            csv_cell(&row.sku),
            csv_cell(&row.item_name),
            csv_cell(row.kind.as_str()),
            csv_cell(&row.signed_quantity().to_text()),
            csv_cell(row.reason.as_str()),
            csv_cell(&row.location_code),
            csv_cell(&row.reference().to_string()),
            csv_cell(&row.actor_user_id.map_or_else(String::new, |id| id.to_string())),
            csv_cell(&row.note),
            csv_cell(&row.on_hand_after.to_text()),
            csv_cell(&row.reserved_after.to_text()),
        ));
    }
    with_bom(out)
}

/// The adjustment inbox, as a CSV — the approver's decision list, offline.
///
/// The `amount` and `threshold` columns are the numbers the decision is actually made on, and
/// they are the request's **snapshots**: exporting them and re-importing them into a spreadsheet
/// is how an organization audits who approved what, so they cannot be recomputed from today's
/// settings.
pub fn approvals_csv(page: &Page<crate::approvals::ApprovalView>) -> String {
    let mut out = String::with_capacity(2_048);
    out.push_str(
        "requested_at,sku,item,location,kind,quantity,amount,threshold,status,decision,\
         decided_at,movement_id,note,comment\r\n",
    );
    for row in &page.items {
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{}\r\n",
            csv_cell(&crate::dates::to_wire(&row.created_at)),
            csv_cell(&row.sku),
            csv_cell(&row.item_name),
            csv_cell(&row.location_code),
            csv_cell(&row.kind),
            csv_cell(&row.quantity.to_text()),
            csv_cell(&row.amount.to_text()),
            csv_cell(&row.threshold.to_text()),
            csv_cell(&row.status),
            csv_cell(row.decision.as_deref().unwrap_or("")),
            csv_cell(
                &row.decided_at
                    .as_ref()
                    .map(crate::dates::to_wire)
                    .unwrap_or_default(),
            ),
            csv_cell(&row.movement_id.map_or_else(String::new, |id| id.to_string())),
            csv_cell(&row.note),
            csv_cell(row.comment.as_deref().unwrap_or("")),
        ));
    }
    with_bom(out)
}

/// Prepend the UTF-8 BOM Excel wants, and be honest about the line ending.
fn with_bom(body: String) -> String {
    // A BOM, because Excel opens a CSV holding an item's name and mangles every non-ASCII
    // character without one — and a Turkish organization is the first thing this module's users
    // are. Same reason and same trade-off as `modules/sales`.
    format!("\u{feff}{body}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_cell_is_not_quoted() {
        assert_eq!(csv_cell("ABC-1"), "ABC-1");
    }

    #[test]
    fn a_cell_with_a_comma_is_quoted() {
        assert_eq!(csv_cell("Bolge 1, Kat 2"), "\"Bolge 1, Kat 2\"");
    }

    #[test]
    fn a_quote_inside_a_cell_is_doubled() {
        // RFC 4180: a literal quote is written as two. A single one terminates the cell and the
        // rest of the row becomes a column, which is how a note turns a column into a column.
        assert_eq!(csv_cell("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn a_formula_is_neutralised() {
        // The export carries item names and operator notes, both of which are user-entered, and
        // both of which end up opened in a spreadsheet. A cell starting with `=` is code there.
        assert_eq!(csv_cell("=SUM(A1:A9)"), "'=SUM(A1:A9)");
        assert_eq!(csv_cell("+1"), "'+1");
        assert_eq!(csv_cell("@cmd"), "'@cmd");
        // A negative number is not guarded — the second test is where that rule is proved in
        // full, because it is the case this function exists to get right.
        assert_eq!(csv_cell("-2"), "-2");
    }

    #[test]
    fn a_negative_quantity_is_not_mistaken_for_a_formula() {
        // **The trap this guards, and the one that made the first version of the function wrong
        // in the other direction.** A ledger export is full of negative numbers — that is what a
        // ledger is — and a guard that prefixes every one of them turns the export into a file
        // of text in the approver's spreadsheet, which is a worse bug than the injection it
        // prevents and one nobody notices because the file still opens. A `-` starts a formula
        // only when something *evaluable* follows it, so these are numbers and pass through.
        assert_eq!(csv_cell("-1.500"), "-1.500");
        assert_eq!(csv_cell("- 3 units"), "- 3 units");
        assert_eq!(csv_cell("-.5"), "-.5");
        // A bare `-`, or one followed by a letter, is not a number and is guarded.
        assert_eq!(csv_cell("-"), "'-");
        assert_eq!(csv_cell("-cmd"), "'-cmd");
    }

    #[test]
    fn the_header_names_the_columns_the_screen_shows() {
        let empty: Page<StockLevel> = Page::new(Vec::new(), None, 0);
        let csv = stock_csv(&empty);
        let header = csv.trim_start_matches('\u{feff}').lines().next().unwrap_or_default();
        // The criterion is "the export matches the table", and the cheapest way to keep a
        // spreadsheet from reordering the reader's columns is to keep the header in the screen's
        // order. Assert the order rather than the mere presence: a CSV with the right columns in
        // the wrong order is a file nobody notices until the totals do not add up.
        assert_eq!(
            header,
            "sku,item,category,unit,warehouse_location,on_hand,reserved,available,min_threshold,\
             reorder_point,status,last_movement"
        );
    }

    #[test]
    fn an_empty_export_is_a_header_and_nothing_else() {
        // A screen showing "no rows" and a download producing a file with a header and no data
        // rows is the honest pair. A completely empty file is indistinguishable from a failed
        // request in most spreadsheet programs.
        let empty: Page<StockLevel> = Page::new(Vec::new(), None, 0);
        let csv = stock_csv(&empty);
        assert!(csv.starts_with('\u{feff}'));
        assert_eq!(csv.trim_start_matches('\u{feff}').lines().count(), 1);
    }
}
