//! The write-only provider credential (REQ-011, slice 4).
//!
//! `cdn_settings.credential_ciphertext` is a `bytea` column the API never returns and the
//! panel never renders back. Until this module the column was *also* never written: the
//! settings screen offers a `Replace credential` field, sends it, and the handler dropped it
//! on the floor — a button that reported success and stored nothing, with `has_credential`
//! pinned to `false` for ever. The screen shipped working; the thing behind it did not exist.
//!
//! ## Why the envelope lives in the CDN crate and not in the route
//!
//! The credential is read on the **purge worker's** path (`provider_for_site`), not only on
//! the settings path, and a worker that cannot decrypt what the settings screen encrypted
//! would be a credential that saves successfully and then never authenticates. One module,
//! one round trip, used by both, is the only shape where the two halves cannot disagree.
//!
//! ## The envelope
//!
//! Encrypt-then-MAC over the primitives the platform already ships, reusing
//! `omnion_identity::secrets::SecretBox` rather than adding a dependency for one column: the
//! platform's answer to "how is a stored secret protected" is already written down in
//! `crates/identity/src/secrets.rs`, with its key material, its development fallback and its
//! failure mode. A second, differently-shaped envelope for the same database would be two
//! answers to one question, and a key rotation would have to know about both.
//!
//! The column holds the **envelope string**, not raw bytes. `bytea` was the schema's choice
//! and the choice is kept — an envelope is bytes, and storing the text of a versioned format
//! in a binary column is what makes the version check (`v1.`) possible on read.

use omnion_identity::secrets::SecretBox;

use crate::error::CdnError;

/// The prefix a stored envelope always carries.
///
/// It is a *stored* column rather than a length heuristic, so a value written by hand, or by
/// an older build, is recognisable without a decryption attempt and without guessing.
const ENVELOPE_PREFIX: &str = "omnion-cdn-credential.v1:";

/// Seal a credential for storage.
///
/// # Errors
///
/// Refuses a credential that is empty or longer than the column's real limit. Silently
/// storing an empty envelope is how `has_credential` becomes a lie: a row would report a
/// stored credential and every adapter would then send an empty bearer token.
pub fn seal(box_: &SecretBox, credential: &str) -> Result<Vec<u8>, CdnError> {
    let trimmed = credential.trim();
    if trimmed.is_empty() {
        return Err(CdnError::InvalidCredential(
            "the credential is empty — a blank field keeps the stored credential rather than \
             replacing it with nothing"
                .to_string(),
        ));
    }
    // 2048 bytes of envelope is a 2 KiB provider key, which no provider issues. Anything
    // larger is a paste accident, and the refusal is better than a `bytea` that silently
    // truncated on a future migration.
    if trimmed.len() > MAX_CREDENTIAL_BYTES {
        return Err(CdnError::InvalidCredential(format!(
            "the credential is {} bytes; this column holds at most {}",
            trimmed.len(),
            MAX_CREDENTIAL_BYTES
        )));
    }
    let envelope = box_.encrypt(trimmed.as_bytes());
    Ok(format!("{ENVELOPE_PREFIX}{envelope}").into_bytes())
}

/// Open a stored envelope back into the credential.
///
/// Returns `None` for a column that is empty, `Some(Err(..))` for one that holds something
/// this build cannot read. The distinction is the point: "no credential configured" is a
/// state an adapter reports honestly and a purge still queues, while "a credential is
/// configured and this process cannot read it" is a key that must be reported rather than
/// dropped — it is the difference between an operator being told to paste their key again and
/// an operator being told their key is being rejected for an invisible reason.
pub fn open(box_: &SecretBox, stored: &[u8]) -> Option<Result<String, CdnError>> {
    let raw = std::str::from_utf8(stored).ok()?;
    let envelope = raw.strip_prefix(ENVELOPE_PREFIX)?;
    Some(
        box_.decrypt(envelope)
            .map_err(|_| {
                CdnError::CredentialUnreadable(
                    "a CDN credential is stored for this site but this process cannot decrypt \
                     it — the secret key it was sealed with is not the one now in use"
                        .to_string(),
                )
            })
            // The plaintext is the operator's key: a non-UTF-8 one is a corrupt row, and
            // returning bytes here would put them in a header.
            .and_then(|bytes| {
                String::from_utf8(bytes).map_err(|_| {
                    CdnError::CredentialUnreadable(
                        "the stored CDN credential is not text — treat it as a corrupt row and \
                         replace the credential"
                            .to_string(),
                    )
                })
            }),
    )
}

/// The cap [`seal`] enforces, re-exported so a caller can validate before encrypting.
pub const MAX_CREDENTIAL_BYTES: usize = 2_048;

#[cfg(test)]
mod tests {
    use super::*;

    fn box_() -> SecretBox {
        SecretBox::from_key_material(b"cdn-credential-test-key")
    }

    #[test]
    fn a_sealed_credential_opens_back_to_exactly_what_was_typed() {
        let sealed = seal(&box_(), "  cdn-live-key-abc123  ").expect("seal must succeed");
        let opened = open(&box_(), &sealed).expect("a sealed column is present");
        assert_eq!(
            opened.expect("seal produced a readable envelope"),
            "cdn-live-key-abc123"
        );
    }

    #[test]
    fn the_stored_bytes_never_contain_the_credential() {
        let sealed = seal(&box_(), "cdn-live-key-abc123").expect("seal must succeed");
        let text = String::from_utf8(sealed).expect("the envelope is text");
        assert!(
            !text.contains("cdn-live-key"),
            "the stored column must not hold the plaintext: {text}"
        );
        assert!(
            text.starts_with(ENVELOPE_PREFIX),
            "a stored value must name its format, or a value written by another build reads as \
             a corrupt row instead of as a credential: {text}"
        );
    }

    #[test]
    fn the_same_credential_twice_is_two_different_rows() {
        // A fresh nonce per encryption: an operator who pastes the same key twice must not
        // leave two rows an observer can match by comparison.
        let first = seal(&box_(), "same-key").expect("seal must succeed");
        let second = seal(&box_(), "same-key").expect("seal must succeed");
        assert_ne!(
            first, second,
            "identical plaintexts must not produce identical rows"
        );
    }

    #[test]
    fn a_column_written_by_another_key_reports_a_refusal_rather_than_an_empty_credential() {
        let sealed =
            seal(&SecretBox::from_key_material(b"the-other-key"), "k").expect("seal must succeed");
        let opened = open(&box_(), &sealed).expect("a sealed column is present");
        let message = match opened {
            Ok(value) => panic!("a wrong key must not open the envelope, got {value:?}"),
            Err(error) => error.to_string(),
        };
        assert!(
            message.contains("cannot decrypt"),
            "the refusal has to name the cause: {message}"
        );
    }

    #[test]
    fn a_column_holding_something_that_is_not_an_envelope_reads_as_no_credential() {
        // `None`, not an error: a row written before this build has raw bytes in that
        // column, and treating it as a corrupt row would fail every purge on an
        // installation that has never stored a credential through the panel.
        assert!(
            open(&box_(), b"").is_none(),
            "an empty column is no credential"
        );
        assert!(
            open(&box_(), b"\x00\x01\x02").is_none(),
            "non-UTF-8 is not an envelope"
        );
        assert!(open(&box_(), b"plain text somebody typed in psql").is_none());
    }

    #[test]
    fn a_blank_or_oversized_credential_is_refused_with_the_reason() {
        let blank = seal(&box_(), "   ").expect_err("a blank credential must be refused");
        assert!(
            blank.to_string().contains("empty"),
            "the refusal names the cause: {blank}"
        );

        let huge = "k".repeat(MAX_CREDENTIAL_BYTES + 1);
        let oversized = seal(&box_(), &huge).expect_err("an oversized credential must be refused");
        assert!(
            oversized.to_string().contains("at most"),
            "the refusal carries the cap: {oversized}"
        );

        assert!(
            seal(&box_(), &"k".repeat(MAX_CREDENTIAL_BYTES)).is_ok(),
            "the cap is inclusive — a test that only tried the refusal would not notice an \
             off-by-one that locked out a legitimate maximum"
        );
    }
}
