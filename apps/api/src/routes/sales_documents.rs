//! `GET /api/v1/sales/quotes/{id}/pdf` and `GET /api/v1/sales/orders/{id}/pdf`
//! (docs/requests/REQ-052, slice 5).
//!
//! Like the report export in [`crate::routes::sales_reports`], this file is a **file download**
//! rather than a JSON body, and it inherits that route's reason for the two being separate: a
//! browser that expected a document and got `{"error": …}` has nothing to show a person, and a
//! route that can answer either way has whichever error handling ran last. It is the second
//! download in the module, and it copies the good parts of the first deliberately.
//!
//! Four decisions are worth stating, because each of them is a way this could have been wrong:
//!
//! * **The PDF is a read, not a document mutation.** It is mounted on the same key as the detail
//!   (`sales.quotes.read` / `sales.orders.read`) and writes **no audit row**, because a seller
//!   clicking "Download PDF" on a screen they are already looking at has done nothing that needs
//!   to be in a compliance trail — and a read key that silently produced audit rows would make
//!   the trail unreadable within a week. The report export took the same decision, and this is
//!   why that rule is a rule and not a coincidence.
//!
//! * **The read is the detail read, not a document of its own.** A separate "pdf rows" query
//!   would be a second implementation of "which lines belong to this quote, in what order, with
//!   what frozen totals", and the one that drifts is the one nobody looks at. The document
//!   renders [`QuoteDetail`] and [`OrderDetail`] verbatim, so the PDF cannot disagree with the
//!   screen it was downloaded from — and the two ways to make them disagree (a different status
//!   filter, a different date) are both unreachable from one call.
//!
//! * **The expiry sweep runs first**, exactly as it does on `GET /sales/quotes/{id}`. A quote
//!   that lapsed overnight would otherwise be printed with a live face: the sweep is what makes
//!   "expired" a fact rather than a label, and a PDF is the one artifact that leaves the building
//!   and gets answered to later. Skipping it here would be the module's own rule applying on one
//!   screen and not the next.
//!
//! * **A character the base-14 fonts cannot draw is announced in a response header**, not hidden.
//!   `x-omnion-document-degraded` carries the count. The *sender* is the only person who can act
//!   on it, and the person who receives the file has no way to learn that their own name printed
//!   as something else — so the warning belongs on the way out, and the document is still
//!   returned rather than refused: a seller who cannot download at all has lost a sale over a
//!   letter, which is a worse outcome than a degraded PDF and a told sender.

use axum::extract::{Path, Query, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use uuid::Uuid;

use omnion_module_sales::documents::{self, Rendered};
use omnion_module_sales::orders;
use omnion_module_sales::quotes;

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::routes::crm::organization_of;
use crate::routes::sales_reports::OrganizationParam;
use crate::state::AppState;

/// A file, a JSON body or a refusal — whichever the route produced.
pub type Response = axum::response::Response;

/// The header that tells the sender the document could not print every character it was given.
const DEGRADED: &str = "x-omnion-document-degraded";

/// `GET /api/v1/sales/quotes/{id}/pdf` — the quotation as a document.
pub async fn quote_pdf(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    Path(quote_id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();
    // The same sweep the detail screen runs, and for the same reason: a lapsed quotation must not
    // be printable with a live face.
    let _ = quotes::sweep_expired(pool, organization_id).await;
    let detail = quotes::get_quote(pool, organization_id, quote_id).await?;
    let organization_name = organization_name(&state, organization_id).await;
    Ok(pdf_response(documents::render_quote(
        &detail,
        &organization_name,
    )))
}

/// `GET /api/v1/sales/orders/{id}/pdf` — the order confirmation as a document.
pub async fn order_pdf(
    State(state): State<AppState>,
    current: CurrentSession,
    Query(organization): Query<OrganizationParam>,
    Path(order_id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let organization_id = organization_of(&state, &current, organization.organization_id).await?;
    let pool = state.db().pool();
    let detail = orders::get_order(pool, organization_id, order_id).await?;
    let organization_name = organization_name(&state, organization_id).await;
    Ok(pdf_response(documents::render_order(
        &detail,
        &organization_name,
    )))
}

/// The organization's own name, for the letterhead.
///
/// **Optional, and the document says so rather than printing nothing.** An organization row always
/// has a name, so this read is not expected to fail — but a letterhead that silently vanished
/// would be a document that could not be attributed to anybody, and the failure that causes it
/// (a row deleted under a live session, a read that times out) is exactly the kind that happens
/// to the one download a seller is doing in front of a customer. A name that could not be read
/// prints as the word that says so, which a reader can act on.
async fn organization_name(state: &AppState, organization_id: Uuid) -> String {
    sqlx::query_scalar::<_, String>("select name from organizations where id = $1")
        .bind(organization_id)
        .fetch_optional(state.db().pool())
        .await
        .ok()
        .flatten()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "(organization name unavailable)".to_string())
}

/// The download response: the file, its name, and how degraded it is.
///
/// `Content-Disposition` carries a `filename` **and** a `filename*` (RFC 5987). The plain one is
/// for every browser that has ever existed; the starred one is the only way to carry a name with
/// a non-ASCII character in it without corrupting the header. An organization's name is free
/// text and a Turkish one is entirely non-ASCII, so dropping the starred form would produce a
/// file called `Sirkket.pdf` for a company called "Şirket" — which is the same substitution the
/// document layer went to such lengths to *report*, reintroduced in the file name.
fn pdf_response(rendered: Rendered) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/pdf"));
    // `nosniff` because a file that a browser will happily render as HTML is a stored-XSS
    // primitive, and a download route is the one place the content type is chosen by us rather
    // than derived from a path.
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    let ascii = rendered
        .filename
        .chars()
        .map(|ch| if ch.is_ascii() { ch } else { '_' })
        .collect::<String>();
    headers.insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "attachment; filename=\"{ascii}\"; filename*=UTF-8''{}",
            percent_encode(&rendered.filename)
        ))
        .unwrap_or_else(|_| HeaderValue::from_static("attachment; filename=\"document.pdf\"")),
    );
    if rendered.degraded_characters > 0 {
        if let Ok(value) = HeaderValue::from_str(&rendered.degraded_characters.to_string()) {
            headers.insert(DEGRADED, value);
        }
    }
    (StatusCode::OK, headers, rendered.bytes).into_response()
}

/// Percent-encode a file name for the RFC 5987 form, keeping the characters that form allows.
///
/// `attr-char` is everything except the delimiters and anything non-ASCII, so a name that is
/// already plain ASCII passes through untouched and a Turkish one is encoded byte by byte.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        let ch = *byte as char;
        if ch.is_ascii_alphanumeric() || "-._~".contains(ch) {
            out.push(ch);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_name_survives_percent_encoding_unchanged() {
        assert_eq!(
            percent_encode("quote-Q-2026-0001-Ferman.pdf"),
            "quote-Q-2026-0001-Ferman.pdf"
        );
    }

    #[test]
    fn a_turkish_name_is_encoded_byte_by_byte() {
        // "Şirket" is two UTF-8 bytes that are not URL-safe, and this is the only form that can
        // carry them without corrupting the header.
        let encoded = percent_encode("Şirket.pdf");
        assert!(
            encoded.is_ascii(),
            "the encoded form must be ASCII: {encoded}"
        );
        assert_eq!(encoded, "%C5%9Eirket.pdf");
    }

    #[test]
    fn a_space_and_a_slash_are_escaped_rather_than_left_to_break_the_header() {
        let encoded = percent_encode("a b/c.pdf");
        assert!(
            !encoded.contains(' '),
            "a raw space truncates the filename in some clients"
        );
        assert!(!encoded.contains('/'), "a slash would read as a path");
        assert_eq!(encoded, "a%20b%2Fc.pdf");
    }

    #[test]
    fn a_response_is_a_pdf_with_a_name_and_no_sniff() {
        let rendered = Rendered {
            bytes: b"%PDF-1.4\n%%EOF\n".to_vec(),
            filename: "quote-Q-2026-0001.pdf".into(),
            degraded_characters: 0,
        };
        let response = pdf_response(rendered);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CONTENT_TYPE], "application/pdf");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        let disposition = response.headers()[CONTENT_DISPOSITION].to_str().unwrap();
        assert!(disposition.contains("attachment"), "{disposition}");
        assert!(
            disposition.contains("quote-Q-2026-0001.pdf"),
            "{disposition}"
        );
    }

    #[test]
    fn a_degraded_document_says_so_and_a_clean_one_does_not() {
        // The header is the whole point: a sender who is not told cannot know the file is
        // degraded, and a header that fires on every file teaches people to ignore it.
        let mut rendered = Rendered {
            bytes: b"%PDF-1.4".to_vec(),
            filename: "quote.pdf".into(),
            degraded_characters: 0,
        };
        assert!(
            pdf_response(rendered.clone())
                .headers()
                .get(DEGRADED)
                .is_none(),
            "a clean document must not claim to be degraded"
        );
        rendered.degraded_characters = 3;
        let header = pdf_response(rendered).headers().get(DEGRADED).cloned();
        assert_eq!(header.expect("a degraded document is announced"), "3");
    }
}
