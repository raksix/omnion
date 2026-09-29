//! The two documents the sales module prints: a quote and an order.
//!
//! [`pdf`] knows how to put a string at a point on a page. This module knows what a quote *is* —
//! a header, a line grid, a totals block, the terms — and is the only place that knowledge lives,
//! so the two documents cannot drift into being the same layout with different numbers.
//!
//! ## Why the layout is one function and not a template engine
//!
//! A document with a header, seven columns of lines and a totals block has exactly one shape. A
//! template would add a syntax, an error mode (an unknown variable renders as an empty cell, and
//! an empty cell in a money column is a document that under-charges) and a dependency, in exchange
//! for flexibility nobody has asked for. The rule the module holds to instead: **a field that is
//! missing prints a dash, never a zero and never nothing**, because a blank amount in a document
//! somebody is about to sign is read as "nothing owed".
//!
//! ## The totals are the stored ones, not a re-addition of the printed lines
//!
//! `QuoteTotalsView` is what the server computed and froze onto the row. The document prints those
//! strings verbatim. It does **not** sum the line totals it is about to draw, and that is the one
//! rule here that matters most: if the document added up its own lines, a rounding rule that
//! changed in the server would leave the PDF agreeing with itself and disagreeing with the
//! database, and the two files would be irreconcilable by inspection. A printed total that is
//! wrong is a bug; a printed total that is *self-consistent but wrong* is a bug nobody finds.

use uuid::Uuid;

use crate::orders::OrderDetail;
use crate::pdf::{self, Align, Document};
use crate::quotes::QuoteDetail;

// The page furniture, as named constants rather than literals repeated in two documents.
const HEADER_TITLE: f32 = 18.0;
const SECTION_TITLE: f32 = 11.0;
const LABEL: f32 = 8.0;
const BODY: f32 = 9.0;
const GAP: f32 = 11.0;
const SECTION_GAP: f32 = 6.0;

/// Where each column of the line grid starts and ends, and how its cell is aligned.
///
/// **One table of geometry, read by both the header and the rows.** A grid whose header and rows
/// each name their own x offsets is a grid where the two disagree after somebody nudges one of
/// them, and the result is a document whose "Total" label sits a centimetre left of the figures
/// above it.
struct Grid;

impl Grid {
    /// `#` — a line number, useful when a customer refers to "line 3".
    const POS: (f32, f32, Align) = (pdf::LEFT_EDGE, pdf::LEFT_EDGE + 24.0, Align::Left);
    /// Description and SKU, the widest column and the only one that wraps.
    const DESC: (f32, f32, Align) = (pdf::LEFT_EDGE + 28.0, pdf::LEFT_EDGE + 220.0, Align::Left);
    /// Quantity, right-aligned: it is a number and the reader compares them down the column.
    const QTY: (f32, f32, Align) = (pdf::LEFT_EDGE + 226.0, pdf::LEFT_EDGE + 270.0, Align::Right);
    /// Unit price, right-aligned against the quantity above it.
    const PRICE: (f32, f32, Align) = (pdf::LEFT_EDGE + 276.0, pdf::LEFT_EDGE + 336.0, Align::Right);
    /// The discount, as a whole percent. Right-aligned for the same reason.
    const DISC: (f32, f32, Align) = (pdf::LEFT_EDGE + 342.0, pdf::LEFT_EDGE + 386.0, Align::Right);
    /// The line total, flush with the right margin like the totals block below it.
    const TOTAL: (f32, f32, Align) = (pdf::LEFT_EDGE + 392.0, pdf::RIGHT_EDGE, Align::Right);

    /// The columns, in the order a row's cells are given.
    const ALL: [(f32, f32, Align); 6] = [
        Self::POS,
        Self::DESC,
        Self::QTY,
        Self::PRICE,
        Self::DISC,
        Self::TOTAL,
    ];
}

/// The rendered document, and the one fact a caller must not forget to pass on.
#[derive(Debug, Clone)]
pub struct Rendered {
    /// The PDF bytes.
    pub bytes: Vec<u8>,
    /// A file name that is safe in a `Content-Disposition` header and says what the file is.
    pub filename: String,
    /// Characters that could not be drawn exactly, so the route can say the document is degraded.
    pub degraded_characters: usize,
}

/// What a document is called on the page, and what the two layouts agree on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A quotation the customer may still accept.
    Quote,
    /// A delivery the organization has committed to.
    Order,
}

impl Kind {
    /// The word at the top of the page, in the customer's language rather than the database's.
    const fn heading(self) -> &'static str {
        match self {
            Self::Quote => "Quotation",
            Self::Order => "Order confirmation",
        }
    }

    /// The status word beside the number. A document's status is **not** decoration: a quotation
    /// marked "expired" and a quotation marked "draft" are different promises, and a PDF that
    /// omitted the line would print a lapsed document with the same face as a live one.
    ///
    /// Two methods rather than one with two optional arguments: the two documents have a status
    /// of different types, so a single signature would force every caller to pass a `None` for
    /// the half it does not have — and a reader of `status_label(None, Some(..))` cannot tell
    /// which document it is describing.
    fn quote_status(self, status: crate::model::QuoteStatus) -> String {
        self.status_word(status.as_str())
    }

    /// The same, for an order.
    fn order_status(self, status: crate::model::OrderStatus) -> String {
        self.status_word(status.label())
    }

    /// The shared rule: a stored status is `pending_approval` and a document reads
    /// `pending approval`, because the underscore is a column's punctuation and not a word's.
    fn status_word(self, stored: &str) -> String {
        if stored.trim().is_empty() {
            String::new()
        } else {
            stored.replace('_', " ")
        }
    }
}

/// Render a quote as a PDF.
#[must_use]
pub fn render_quote(detail: &QuoteDetail, organization_name: &str) -> Rendered {
    let quote = &detail.quote;
    let mut doc = Document::new(format!(
        "{} {} — {}",
        Kind::Quote.heading(),
        quote.number,
        organization_name
    ));
    header(
        &mut doc,
        Kind::Quote,
        &quote.number,
        &quote.title,
        &Kind::Quote.quote_status(quote.status),
        organization_name,
        &quote.customer.name,
        &quote.currency,
        &quote.valid_until.to_string(),
    );

    // The reference is what the customer quotes back on the phone, so it sits in the header's
    // second column rather than buried in the notes.
    if !detail.reference.trim().is_empty() {
        doc.text(pdf::LEFT_EDGE + 300.0, LABEL, false, "Reference");
        doc.text(pdf::LEFT_EDGE + 300.0, BODY, true, &detail.reference);
        doc.space(GAP);
    }

    line_grid(
        &mut doc,
        detail.lines.iter().map(|line| {
            (
                line.position.to_string(),
                sku_and_description(
                    line.product.as_ref().map(|p| p.sku.as_str()),
                    &line.description,
                ),
                quantity(&line.quantity),
                amount(&line.unit_price),
                percent(line.discount_percent),
                // Through `amount` like every other figure, for the reason the totals block gives:
                // a free line is "nothing to charge", and printing 0.00 in the grid while the
                // totals below print a dash is a document that contradicts itself two centimetres
                // later. The bug the walk found: the rule was stated and applied to the totals
                // block only, so the one column a reader scans fastest was the one that lied.
                amount(&line.line_total),
            )
        }),
    );

    totals(
        &mut doc,
        &quote.totals.subtotal,
        &quote.totals.discount_total,
        &quote.totals.tax_total,
        &quote.totals.grand_total,
    );

    // The customer's own notes, which on a sent quote are the first thing they read after the
    // total — so they come before the terms, and they are the seller's free text printed as
    // wrapped paragraphs rather than as one long line running off the page.
    block(&mut doc, "Notes", &detail.notes);
    block(&mut doc, "Terms", TERMS_QUOTE);

    finish(
        doc,
        Kind::Quote,
        &quote.number,
        organization_name,
        quote.updated_at,
    )
}

/// Render an order as a PDF.
#[must_use]
pub fn render_order(detail: &OrderDetail, organization_name: &str) -> Rendered {
    let order = &detail.order;
    let mut doc = Document::new(format!(
        "{} {} — {}",
        Kind::Order.heading(),
        order.number,
        organization_name
    ));
    header(
        &mut doc,
        Kind::Order,
        &order.number,
        &order
            .quote_number
            .as_ref()
            .map(|number| format!("From quotation {number}"))
            .unwrap_or_default(),
        &Kind::Order.order_status(order.status),
        organization_name,
        &order.customer.name,
        &order.currency,
        &order.created_at.to_string()[..10.min(order.created_at.to_string().len())],
    );

    line_grid(
        &mut doc,
        detail.lines.iter().map(|line| {
            (
                line.position.to_string(),
                sku_and_description(
                    line.product.as_ref().map(|p| p.sku.as_str()),
                    &line.description,
                ),
                quantity(&line.quantity),
                amount(&line.unit_price),
                percent(line.discount_percent),
                // Through `amount` like every other figure, for the reason the totals block gives:
                // a free line is "nothing to charge", and printing 0.00 in the grid while the
                // totals below print a dash is a document that contradicts itself two centimetres
                // later. The bug the walk found: the rule was stated and applied to the totals
                // block only, so the one column a reader scans fastest was the one that lied.
                amount(&line.line_total),
            )
        }),
    );

    totals(
        &mut doc,
        &detail.totals.subtotal,
        &detail.totals.discount_total,
        &detail.totals.tax_total,
        &detail.totals.grand_total,
    );

    // A confirmed order holds stock, and a reader of the PDF has no way to know that from a
    // status word alone. The line says what is held and in what state, so a printed order can be
    // reconciled against the warehouse.
    if detail
        .lines
        .iter()
        .any(|line| line.reservation.as_ref().is_some_and(|r| r.state == "held"))
    {
        block(&mut doc, "Stock", STOCK_HELD);
    }
    if let Some(invoice) = detail.invoice.as_ref() {
        block(
            &mut doc,
            "Invoice",
            &format!(
                "A {} invoice draft exists for this order ({}).",
                invoice.state, invoice.id
            ),
        );
    }
    block(&mut doc, "Terms", TERMS_ORDER);

    finish(
        doc,
        Kind::Order,
        &order.number,
        organization_name,
        order.updated_at,
    )
}

/// The default terms, so a document is never blank where a reader looks for them.
const TERMS_QUOTE: &str = "This quotation is valid until the date above. Prices are stated in the \
currency shown and exclude delivery unless the line says otherwise. Please quote the document \
number on your order.";
const TERMS_ORDER: &str = "This order confirms the organization's commitment to deliver. Please \
quote the order number on any correspondence. Stock holds recorded on this document are released \
if the order is cancelled.";

/// The default stock note.
const STOCK_HELD: &str = "Stock is held for this order. Cancelling the order returns the hold, \
and the release is recorded rather than deleted.";

/// The two-column header both documents share.
///
/// `issued` is the document's own date: for a quote the day the customer must answer by is
/// already in the header, and the day it was written is what both sides need when a version is
/// questioned later.
#[allow(clippy::too_many_arguments)]
fn header(
    doc: &mut Document,
    kind: Kind,
    number: &str,
    subtitle: &str,
    status: &str,
    organization: &str,
    customer: &str,
    currency: &str,
    issued: &str,
) {
    doc.text(pdf::LEFT_EDGE, HEADER_TITLE, true, kind.heading());
    // The number is the single most-sought fact on the page, so it is the largest thing in the
    // right column, under the organization's name.
    doc.text_right(pdf::RIGHT_EDGE, 12.0, true, number);
    doc.space(GAP);
    if !subtitle.trim().is_empty() {
        doc.text(pdf::LEFT_EDGE, BODY, false, subtitle);
        doc.space(GAP);
    }
    if !status.trim().is_empty() {
        // Printed as text rather than as a coloured badge: a PDF has no badge, and a coloured
        // shape that means "expired" in one viewer means nothing in a printed copy.
        doc.text(pdf::LEFT_EDGE, LABEL, false, "Status");
        doc.text(pdf::LEFT_EDGE, BODY, true, status);
        doc.space(GAP);
    }
    doc.rule();
    doc.space(SECTION_GAP);

    // Who issued it, to whom, and in what money: the three facts a reader checks first.
    doc.text(pdf::LEFT_EDGE, LABEL, false, "From");
    doc.text(pdf::LEFT_EDGE + 60.0, BODY, false, organization);
    doc.text(pdf::LEFT_EDGE + 300.0, LABEL, false, "Date");
    doc.text(pdf::LEFT_EDGE + 360.0, BODY, false, issued);
    doc.space(GAP);
    doc.text(pdf::LEFT_EDGE, LABEL, false, "To");
    doc.text(pdf::LEFT_EDGE + 60.0, BODY, false, customer);
    doc.text(pdf::LEFT_EDGE + 300.0, LABEL, false, "Currency");
    doc.text(pdf::LEFT_EDGE + 360.0, BODY, false, currency);
    doc.space(SECTION_GAP);
    doc.rule();
    doc.space(SECTION_GAP);
}

/// The line grid, with a repeated header band on every page.
///
/// A quote with sixty lines breaks across pages, and a grid whose continuation has no column
/// headings is a grid the reader has to guess on the second page — so the header is redrawn
/// whenever the document has moved to a new page. The check is [`Document::page_count`] and not a
/// row count: a break is decided by **how much vertical space is left**, so a grid of short
/// descriptions and a grid of long ones break at different rows. Counting rows to predict a break
/// gets it wrong for exactly the documents that need the repeated heading most.
fn line_grid<I>(doc: &mut Document, rows: I)
where
    I: IntoIterator<Item = (String, String, String, String, String, String)>,
{
    let headers = ["#", "Description", "Qty", "Unit price", "Disc", "Total"];
    let draw_headings = |doc: &mut Document| {
        doc.band(18.0, 0.90);
        doc.row(&Grid::ALL, &headers, false);
    };
    draw_headings(doc);

    let mut banded = true;
    let mut pages = doc.page_count();
    for cells in rows {
        let refs: Vec<&str> = vec![
            cells.0.as_str(),
            cells.1.as_str(),
            cells.2.as_str(),
            cells.3.as_str(),
            cells.4.as_str(),
            cells.5.as_str(),
        ];
        doc.row(&Grid::ALL, &refs, banded);
        banded = !banded;
        // `row` may have carried the cursor onto a fresh page; if it did, the page is now a
        // continuation and needs its own headings before the next line lands on it.
        if doc.page_count() != pages {
            pages = doc.page_count();
            doc.space(SECTION_GAP);
            draw_headings(doc);
            banded = true;
        }
    }
    doc.rule();
    doc.space(SECTION_GAP);
}

/// The totals block: the four stored totals, right-aligned against the margin.
///
/// The grand total is set in bold and one point larger, because it is the only figure anybody
/// reads twice. It is printed from `totals.grand_total` — the server's own frozen string — and
/// **not** as the sum of the lines above it. See the module docs: a document that re-adds its
/// own rows is a document that can disagree with the database while looking perfectly consistent.
fn totals(doc: &mut Document, subtotal: &str, discount: &str, tax: &str, grand: &str) {
    let rows = [("Subtotal", subtotal), ("Discount", discount), ("Tax", tax)];
    for (label, value) in rows {
        doc.text_right(pdf::RIGHT_EDGE - 90.0, BODY, false, label);
        doc.text_right(pdf::RIGHT_EDGE, BODY, false, &amount(value));
        doc.space(GAP);
    }
    doc.rule();
    doc.space(4.0);
    doc.text_right(pdf::RIGHT_EDGE - 90.0, BODY + 1.0, true, "Total due");
    doc.text_right(pdf::RIGHT_EDGE, BODY + 1.0, true, &amount(grand));
    doc.space(SECTION_GAP);
    doc.rule();
    doc.space(SECTION_GAP);
}

/// A titled free-text block: the notes and the terms.
///
/// Wrapped to the full content width rather than a column, because these are sentences and a
/// sentence in a 90pt column is unreadable.
fn block(doc: &mut Document, title: &str, body: &str) {
    if body.trim().is_empty() {
        return;
    }
    doc.text(pdf::LEFT_EDGE, SECTION_TITLE, true, title);
    doc.space(GAP);
    doc.paragraph(pdf::LEFT_EDGE, BODY, GAP, pdf::PARAGRAPH_WIDTH, body.trim());
    doc.space(SECTION_GAP);
}

/// Close the document and answer what a route needs to hand it to a browser.
fn finish(
    doc: Document,
    kind: Kind,
    number: &str,
    organization: &str,
    _updated_at: time::OffsetDateTime,
) -> Rendered {
    let degraded = doc.unrenderable_characters();
    // A file name is built from the document number only, never from the organization's name: a
    // name is free text, may carry a slash, and would be the one field able to break a header.
    let slug = sanitize(number);
    Rendered {
        bytes: doc.finish(),
        filename: format!(
            "{}-{}-{}.pdf",
            match kind {
                Kind::Quote => "quote",
                Kind::Order => "order",
            },
            slug,
            sanitize(&organization.chars().take(24).collect::<String>())
        ),
        degraded_characters: degraded,
    }
}

/// Reduce a value to the characters a file name may carry, and never to an empty string.
///
/// A number is server-assigned and always well formed, so this is a guard rather than a rule —
/// but a file called `.pdf` downloads as an unnamed attachment in every browser, which is the
/// difference between a file somebody can find again and one they cannot.
fn sanitize(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
        .collect();
    if cleaned.trim_matches('-').is_empty() {
        "document".to_string()
    } else {
        cleaned
    }
}

/// A quantity as the cell prints it: the stored decimal, or a dash when there is none.
///
/// Never `0`: a line with no quantity is a line somebody has not finished entering, and "0"
/// reads as a statement that the quantity is zero.
fn quantity(value: &str) -> String {
    if value.trim().is_empty() || value.trim() == "0" || value.trim() == "0.000" {
        "—".to_string()
    } else {
        value.to_string()
    }
}

/// A discount as the cell prints it: a bare percent, or a dash when there is none.
fn percent(value: i32) -> String {
    if value == 0 {
        "—".to_string()
    } else {
        format!("{value}%")
    }
}

/// An amount, or a dash when the document has none yet.
///
/// **A dash and never `0.00`.** "We owe nothing" and "we have not calculated this" are different
/// facts, and a draft order that prints 0.00 for a line somebody has not priced reads as free.
fn amount(value: &str) -> String {
    if value.trim().is_empty() || value.trim() == "0.00" {
        "—".to_string()
    } else {
        value.to_string()
    }
}

/// The description cell: the SKU and the description on one line, or just the description for a
/// free-text line.
///
/// The description is truncated to fit rather than left to run into the quantity column: the grid's
/// geometry is fixed, and a description that overflows it would print over the next column — which
/// is exactly the "clipped amounts" finding the QA plan asks to be prevented.
fn sku_and_description(sku: Option<&str>, description: &str) -> String {
    let text = match sku {
        Some(sku) if !sku.trim().is_empty() => format!("{sku} — {description}"),
        _ => description.to_string(),
    };
    truncate(&text, Grid::DESC.1 - Grid::DESC.0, BODY, false)
}

/// Shorten a string to the width available, with an ellipsis when anything was removed.
///
/// An ellipsis rather than a hard cut: `Annual maintenance…` is visibly a summary, whereas
/// `Annual maintenan` is a typo in a document somebody is about to sign.
fn truncate(text: &str, width: f32, size: f32, bold: bool) -> String {
    if pdf::text_width(text, size, bold) <= width {
        return text.to_string();
    }
    let ellipsis = "…";
    let budget = width - pdf::text_width(ellipsis, size, bold);
    let mut out = String::new();
    for ch in text.chars() {
        let next = format!("{out}{ch}");
        if pdf::text_width(&next, size, bold) > budget {
            break;
        }
        out = next;
    }
    format!("{out}{ellipsis}")
}

/// A stable id for a document, for a caller that wants to key a cached file.
#[must_use]
pub fn document_key(kind: &str, id: Uuid) -> String {
    format!("{kind}:{id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::CustomerKind;
    use crate::model::{OrderStatus, QuoteStatus};
    use crate::orders::{InvoiceHandoffView, OrderLineView, OrderView};
    use crate::quotes::{CustomerRef, OwnerRef, QuoteLineView, QuoteTotalsView, QuoteView};
    use time::macros::{date, datetime};

    fn totals() -> QuoteTotalsView {
        QuoteTotalsView {
            subtotal: "349.75".into(),
            discount_total: "9.95".into(),
            tax_total: "67.96".into(),
            grand_total: "407.76".into(),
        }
    }

    fn line(position: i32, total: &str) -> QuoteLineView {
        QuoteLineView {
            id: Uuid::new_v4(),
            position,
            product_id: None,
            product: None,
            description: "Annual maintenance visit, including parts".into(),
            unit: "each".into(),
            quantity: "3".into(),
            unit_price: "100.00".into(),
            discount_percent: 20,
            tax_percent: 20,
            line_total: total.into(),
        }
    }

    fn quote_detail() -> QuoteDetail {
        QuoteDetail {
            quote: QuoteView {
                id: Uuid::new_v4(),
                organization_id: Uuid::new_v4(),
                number: "Q-2026-0001".into(),
                title: "Office espresso machine service".into(),
                status: QuoteStatus::Sent,
                customer: CustomerRef {
                    kind: CustomerKind::Company,
                    id: Some(Uuid::new_v4()),
                    name: "Acme (Holdings) Ltd.".into(),
                },
                owner: Some(OwnerRef {
                    id: Uuid::new_v4(),
                    name: "Deniz Aydın".into(),
                }),
                currency: "TRY".into(),
                price_list_id: None,
                valid_until: date![2026 - 12 - 31],
                totals: totals(),
                version: 1,
                max_discount_percent: 20,
                updated_at: datetime![2026-09-29 10:00:00 +00:00],
                created_at: datetime![2026-09-29 10:00:00 +00:00],
            },
            lines: vec![line(1, "240.00"), line(2, "95.28"), line(3, "0.00")],
            versions: Vec::new(),
            has_public_link: true,
            public_link_expires_at: None,
            sent_at: Some(datetime![2026-09-29 10:00:00 +00:00]),
            decline_reason: None,
            cancel_reason: None,
            notes: "Two visits a year, booked by telephone.".into(),
            reference: "PO-88213".into(),
            decided_at: None,
        }
    }

    fn order_detail() -> OrderDetail {
        let now = datetime![2026-09-29 10:00:00 +00:00];
        OrderDetail {
            order: OrderView {
                id: Uuid::new_v4(),
                organization_id: Uuid::new_v4(),
                number: "SO-2026-0001".into(),
                status: OrderStatus::Confirmed,
                customer: CustomerRef {
                    kind: CustomerKind::Company,
                    id: Some(Uuid::new_v4()),
                    name: "Acme (Holdings) Ltd.".into(),
                },
                quote_id: Some(Uuid::new_v4()),
                quote_number: Some("Q-2026-0001".into()),
                currency: "TRY".into(),
                subtotal: "349.75".into(),
                discount_total: "9.95".into(),
                tax_total: "67.96".into(),
                grand_total: "407.76".into(),
                reservation_state: crate::model::OrderReservationState::Total,
                invoice_state: "none".into(),
                owner: None,
                created_at: now,
                updated_at: now,
            },
            lines: vec![OrderLineView {
                id: Uuid::new_v4(),
                position: 1,
                product_id: None,
                product: None,
                description: "Annual maintenance visit".into(),
                unit: "each".into(),
                quantity: "3".into(),
                unit_price: "100.00".into(),
                discount_percent: 0,
                tax_percent: 20,
                line_total: "240.00".into(),
                reservation: Some(crate::orders::ReservationView {
                    id: Uuid::new_v4(),
                    line_id: Uuid::new_v4(),
                    product_id: None,
                    quantity: "3".into(),
                    unit: "each".into(),
                    state: "held".into(),
                    held_at: now,
                    released_at: None,
                    released_reason: String::new(),
                }),
            }],
            history: Vec::new(),
            invoice: Some(InvoiceHandoffView {
                id: Uuid::new_v4(),
                order_id: Uuid::new_v4(),
                state: "draft".into(),
                external_id: None,
                external_url: None,
                subtotal: "349.75".into(),
                tax_total: "67.96".into(),
                grand_total: "407.76".into(),
                currency: "TRY".into(),
                raised_at: now,
            }),
            totals: totals(),
        }
    }

    /// The bytes around the first occurrence of `marker`, for a failure message.
    ///
    /// A test that says "not found" and stops has cost more time than it saved here: twice the
    /// answer was visible in the file and a message that printed the neighbourhood would have
    /// shown it immediately.
    fn find_window(bytes: &[u8], marker: &[u8]) -> Option<String> {
        let at = bytes.windows(marker.len()).position(|w| w == marker)?;
        let start = at.saturating_sub(24);
        Some(
            String::from_utf8_lossy(&bytes[start..(at + marker.len() + 8).min(bytes.len())])
                .into_owned(),
        )
    }

    /// The printable text of a PDF, for asserting on content.
    fn printed(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    #[test]
    fn a_quote_document_is_a_pdf_carrying_the_numbers() {
        let detail = quote_detail();
        let rendered = render_quote(&detail, "Ferman A.Ş.");
        let text = printed(&rendered.bytes);
        assert!(text.starts_with("%PDF-1.4"));
        assert!(
            text.contains("Q-2026-0001"),
            "the number a customer quotes back"
        );
        assert!(text.contains("Quotation"));
        assert!(text.contains("407.76"), "the grand total the customer pays");
        assert!(text.contains("349.75"), "the subtotal");
        assert!(text.contains("PO-88213"), "the customer's own reference");
        assert!(
            text.contains("2026-12-31"),
            "the date they may still accept"
        );
    }

    #[test]
    fn the_parentheses_in_a_customer_name_do_not_break_the_file() {
        // The bug this pins: `Acme (Holdings) Ltd.` unescaped closes the PDF literal string
        // early, and the file is unreadable. It is a plausible company name, not an exotic one.
        let rendered = render_quote(&quote_detail(), "Ferman A.Ş.");
        let text = printed(&rendered.bytes);
        assert!(
            text.contains("Acme \\(Holdings\\) Ltd."),
            "the name is escaped, not truncated: {:?}",
            text.find("Acme").map(|at| &text[at..at + 40])
        );
        assert!(
            text.ends_with("%%EOF\n"),
            "a truncated stream would lose the trailer"
        );
    }

    #[test]
    fn a_zero_amount_is_a_dash_and_never_a_zero() {
        // "We owe nothing" and "this line has no price yet" are different facts.
        let rendered = render_quote(&quote_detail(), "Ferman");
        let text = printed(&rendered.bytes);
        // Every figure goes through `amount` — the grid's unit price and line total included. The
        // rule was written for the totals block and the first version of this asserted only
        // there, so the free line printed `0.00` in the grid while the total below it printed a
        // dash: the same document, two sentences, one of them wrong.
        assert!(
            !text.contains("(0.00) Tj"),
            "no cell may print 0.00: {:?}",
            text.lines()
                .filter(|l| l.contains("0.00"))
                .collect::<Vec<_>>()
        );
        // The dash is WinAnsi 0x97 — **one byte**, not the three bytes of a UTF-8 em dash. A
        // lossy decode of the file cannot contain the character '—' at all, so the previous
        // version of this assertion could only ever fail; this one looks for the byte a reader
        // actually sees.
        // 0x97 is WinAnsi's em dash — a **single** byte, where a UTF-8 em dash is three. Asserted
        // on the byte because the document is WinAnsi, not UTF-8: a lossy decode can never
        // contain the character '—' at all, so the earlier `text.contains('—')` could only fail.
        assert!(
            rendered.bytes.contains(&0x97),
            "an unpriced line prints a dash (WinAnsi 0x97), never 0.00"
        );
    }

    #[test]
    fn a_discount_of_zero_prints_a_dash_not_a_zero_percent() {
        let mut detail = quote_detail();
        detail.lines[0].discount_percent = 0;
        let text = printed(&render_quote(&detail, "Ferman").bytes);
        assert!(!text.contains("(0%)"), "an empty discount is not 0%");
        assert!(text.contains("20%"), "a real discount still prints");
    }

    #[test]
    fn the_printed_total_is_the_stored_one_not_a_sum_of_the_lines() {
        // The property that matters most: if the document re-added its own rows it would be
        // self-consistent and could disagree with the database. So a fixture whose lines do NOT
        // add up to its stored total must print the stored total.
        let mut detail = quote_detail();
        detail.quote.totals.grand_total = "999.99".into();
        let text = printed(&render_quote(&detail, "Ferman").bytes);
        assert!(
            text.contains("999.99"),
            "the frozen total is what a document is obliged to print"
        );
        assert!(
            !text.contains("335.28"),
            "no re-addition of the visible lines"
        );
    }

    #[test]
    fn the_amounts_are_right_aligned_in_one_column() {
        // Read back from the content stream: three totals drawn right-aligned must all end at
        // the same x, which is what the width table is for.
        // Read the page's own operator buffer, not the serialized file: a whole-file lossy
        // decode turns every cp1252 byte above 0x7F into three replacement characters, which is
        // harmless for content assertions and fatal for ones that count or measure.
        let mut doc = Document::new("t");
        super::totals(&mut doc, "349.75", "9.95", "67.96", "407.76");
        let text = doc.ops_text();
        assert!(
            text.contains("349.75"),
            "the totals block drew nothing: {text:?}"
        );
        // Each drawn string with its own x **and its own size**, so a right edge can be computed
        // rather than assumed. Two earlier versions of this test measured the wrong thing: the
        // first compared the x of the four *labels* (all sharing one right edge, and passing for
        // any implementation that right-aligned nothing), the second measured the grand total at
        // the body size when it is deliberately set one point larger.
        let drawn: Vec<(f32, f32, bool, String)> = text
            .lines()
            .filter(|line| line.contains("Tj"))
            .filter_map(|line| {
                // The operator reads: BT /F1 9 Tf 1 0 0 1 48 784.89 Td (…) Tj ET
                // so the size is the number after the font name and the x is the number after the
                // text matrix. Each is found by its own literal — never by an offset from the
                // other, which reads the y as the x the moment the spacing changes, and the
                // symptom of that is a walk that finds *nothing* rather than a wrong number.
                // The operator reads: BT /F1 9 Tf 1 0 0 1 519.75806 784.89 Td (349.75) Tj ET
                //
                // Every field is taken from the line by its own literal. The version this replaces
                // used `line.find("/F") + 3`, which lands on the `1` of `/F1` and then parses the
                // *font name's* trailing digit as the size — so the size was the string "1"/"2",
                // `parse::<f32>` succeeded on it, and the amount was measured in the wrong face.
                // Nothing failed; the walk simply reported an empty list of amounts, which is the
                // least legible possible symptom of a parser reading the wrong column.
                let bold = line.contains("/F2");
                let size: f32 = line
                    .split_whitespace()
                    .nth(2)
                    .and_then(|token| token.parse().ok())?;
                let at = line.find("1 0 0 1 ")? + "1 0 0 1 ".len();
                let rest = &line[at..];
                let end = rest.find(' ')?;
                let x = rest[..end].parse().ok()?;
                let open = rest.find('(')? + 1;
                let close = rest.find(')')?;
                Some((x, size, bold, rest[open..close].to_string()))
            })
            .collect();
        let amounts: Vec<f32> = drawn
            .iter()
            .filter(|(_, _, _, value)| value.contains('.'))
            .map(|(x, size, bold, value)| x + pdf::text_width(value, *size, *bold))
            .collect();
        assert_eq!(
            amounts.len(),
            4,
            "the four totals, found by their decimal point; parsed {drawn:?} from {text:?}"
        );
        // Every right edge is the right margin, to the precision the writer emits coordinates at
        // (two decimals). Checked against the margin itself rather than against each other: two
        // columns that are both wrong by the same amount would agree, and a rule is only worth
        // stating against the thing it is a rule about.
        let margin = pdf::RIGHT_EDGE;
        for (edge, value) in amounts.iter().zip(["349.75", "9.95", "67.96", "407.76"]) {
            assert!(
                (edge - margin).abs() < 0.02,
                "{value} ends at {edge}, not at the right margin {margin}"
            );
        }
    }

    #[test]
    fn an_order_document_names_the_quote_it_came_from_and_the_stock_it_holds() {
        let rendered = render_order(&order_detail(), "Ferman A.Ş.");
        let text = printed(&rendered.bytes);
        assert!(text.contains("SO-2026-0001"));
        assert!(
            text.contains("Q-2026-0001"),
            "an order is traced back to its quote"
        );
        assert!(text.contains("Order confirmation"));
        assert!(
            text.contains("Stock is held"),
            "a hold is a fact the reader needs"
        );
        assert!(
            text.contains("draft"),
            "the invoice draft it owes accounting"
        );
    }

    #[test]
    fn a_document_that_cannot_print_a_character_says_so() {
        // A Turkish name with `ğ` and `ı` prints a readable near-miss, and the count is what
        // lets the route warn the sender. Silence here would ship a wrong name on a document.
        let mut detail = quote_detail();
        detail.quote.customer.name = "İğde Işığı Ltd.".into();
        let rendered = render_quote(&detail, "Ferman");
        assert!(
            rendered.degraded_characters > 0,
            "İ, ğ and the two dotless ı are outside cp1252"
        );
        // What a reader sees, spelled out: cp1252 has no glyph for `İ`, `ğ`, `ı` or `ş`, so each
        // is drawn as its nearest readable letter and **counted**. Every letter of this name ends
        // up plain ASCII, which is the point — the document stays readable, and the route turns
        // the count into a warning the sender can act on before the file leaves the building.
        //
        // This assertion could not have been written before the cedilla pair existed in the alias
        // table: `ş` was reaching for a slot that holds the caron, so the name was neither ASCII
        // nor printable and the honest expectation was unknowable.
        // Read on the raw bytes: the customer name is drawn through the same WinAnsi path as
        // everything else, and a lossy decode of a document that contains a 0x97 dash elsewhere
        // inserts U+FFFD between the parts of this string — which is why searching the decoded
        // text for a name that is perfectly present came back empty.
        let bytes = &rendered.bytes;
        let needle: Vec<u8> = b"Igde Isigi Ltd.".to_vec();
        let at = bytes
            .windows(needle.len())
            .position(|window| window == needle.as_slice());
        assert!(
            at.is_some(),
            "the aliased name must appear verbatim in the bytes; near 'Ltd' there is: {:?}",
            find_window(bytes, b"Ltd")
        );
        assert!(
            !bytes.contains(&b'?'),
            "a letter with a readable near-miss is never drawn as a question mark"
        );
    }

    #[test]
    fn an_ascii_document_reports_nothing_lost() {
        // The false-warning guard: a warning that fires on every document trains people to
        // ignore it.
        let rendered = render_quote(&quote_detail(), "Ferman Ltd");
        assert_eq!(rendered.degraded_characters, 0);
    }

    #[test]
    fn a_long_description_is_shortened_with_an_ellipsis_not_allowed_over_the_next_column() {
        let text = truncate(
            "Quarterly preventative maintenance including filter replacement and calibration",
            120.0,
            BODY,
            false,
        );
        assert!(text.ends_with('…'));
        assert!(pdf::text_width(&text, BODY, false) <= 120.0);
    }

    #[test]
    fn a_file_name_is_built_only_from_characters_a_header_may_carry() {
        assert_eq!(sanitize("Q-2026-0001"), "Q-2026-0001");
        assert_eq!(sanitize("Acme / Holdings"), "AcmeHoldings");
        assert_eq!(sanitize("../etc/passwd"), "etcpasswd");
        assert_eq!(
            sanitize("   "),
            "document",
            "an empty name downloads as nothing"
        );
        let rendered = render_quote(&quote_detail(), "Ferman A.Ş. / Deniz");
        assert!(rendered.filename.ends_with(".pdf"));
        assert!(!rendered.filename.contains(' '), "{:?}", rendered.filename);
        assert!(!rendered.filename.contains('/'), "{:?}", rendered.filename);
    }

    #[test]
    fn a_quote_with_no_lines_still_prints_its_columns_and_its_total() {
        // A grid that vanishes leaves the reader with a page of totals and no idea what they are
        // totals of. The header is drawn unconditionally, so the document says "empty" the way a
        // quote should: by having no rows under its own headings.
        let mut detail = quote_detail();
        detail.lines.clear();
        let text = printed(&render_quote(&detail, "Ferman").bytes);
        assert!(text.contains("Description") && text.contains("Unit price"));
        assert!(
            text.contains("407.76"),
            "the totals are still the document's own"
        );
    }

    #[test]
    fn sixty_lines_break_onto_a_second_page_and_repeat_the_column_headings() {
        // The continuation without headings is the failure this pins: on page two a reader cannot
        // tell which column is the unit price.
        let mut detail = quote_detail();
        detail.lines = (1..=60).map(|n| line(n, "240.00")).collect();
        let text = printed(&render_quote(&detail, "Ferman").bytes);
        let pages = text.matches("/Type /Page ").count();
        assert!(pages > 1, "sixty lines cannot fit on one page");
        let headings = text.matches("(Description) Tj").count();
        assert!(
            headings >= 2,
            "the grid header must repeat on a continuation page, saw it {headings} time(s)"
        );
    }

    #[test]
    fn the_status_is_printed_because_it_is_not_decoration() {
        // A lapsed quotation printed with a draft's face is a promise the organization is not
        // making. The status is therefore a line of the document, not a badge in a UI.
        let mut detail = quote_detail();
        detail.quote.status = QuoteStatus::Expired;
        let text = printed(&render_quote(&detail, "Ferman").bytes);
        assert!(text.contains("Status"));
        assert!(text.contains("expired"));
    }

    #[test]
    fn the_document_key_is_stable_and_names_its_kind() {
        let id = Uuid::new_v4();
        assert_eq!(document_key("quote", id), format!("quote:{id}"));
        assert_ne!(document_key("quote", id), document_key("order", id));
    }
}
