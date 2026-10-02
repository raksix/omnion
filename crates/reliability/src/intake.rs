//! The inbound intake guard: HMAC verification, replay defence, size caps and a narrow
//! sanitisation pass (REQ-127, slice 4).
//!
//! This is the only code in the platform that runs **before** authentication on a route the
//! platform does not control, which is why every part of it is written defensively and why the
//! module's rule is *"refuse cheaply, and never echo"*:
//!
//! * **Size caps are applied before authentication.** An oversized body costs a `413` and one
//!   counter increment, not an HMAC over a hundred megabytes. The order is the contract, and it
//!   is the reason [`evaluate`] checks size before it looks at a signature.
//! * **Signature comparison is constant time** ([`hmac::subtle`]), because a byte-by-byte
//!   comparison leaks the correct prefix and turns a 256-bit tag into a search.
//! * **A rejection never echoes the payload.** It records *why*, never *what* — a rejection log
//!   containing request bodies is a second copy of the data the guard exists to protect.
//!
//! ## The three rejection reasons that are security, not hygiene
//!
//! `signature_invalid` says the bytes changed. `timestamp_stale` says the request is older than
//! the endpoint's tolerance. `replay` says a signature id has been seen inside the tolerance
//! window. The last one needs remembered ids, which is a bounded set with a TTL rather than a
//! table that grows forever — and a signature id that is *inside the body* is what makes it
//! possible, which is why the guard documents that a scheme without an id cannot defend against
//! replay and says so on the screen.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use std::collections::BTreeSet;
use std::net::IpAddr;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::{ReliabilityError, Result};
use crate::vocabulary::{INTAKE_REASONS, INTAKE_SCHEMES, SANITIZE_PROFILES};

/// Floor and ceiling of a declared payload cap.
///
/// 1 KiB is small enough to be a real limit (nothing legitimate is smaller) and 10 MiB is
/// generous enough that a real integration's largest export fits.
pub const MIN_PAYLOAD_BYTES: i32 = 1_024;
/// The largest payload the guard will accept for any endpoint.
pub const MAX_PAYLOAD_BYTES: i32 = 10_485_760;

/// The widest and narrowest replay tolerance, in seconds.
///
/// 30 seconds is tighter than most integrations' clock skew; 3600 is a whole hour, which is a
/// deliberate choice by an operator and not a default.
pub const MIN_TOLERANCE_SECONDS: i32 = 30;
/// An hour.
pub const MAX_TOLERANCE_SECONDS: i32 = 3_600;

/// One declared inbound endpoint's guard configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntakeEndpoint {
    /// The route this declaration guards, e.g. `/api/v1/webhooks/stripe`.
    pub path: String,
    /// An operator-facing name.
    pub name: String,
    /// One of [`INTAKE_SCHEMES`].
    pub hmac_scheme: String,
    /// The header the signature arrives in, e.g. `x-omnion-signature`.
    pub signature_header: String,
    /// The header carrying the unix timestamp, when the scheme has one.
    pub timestamp_header: Option<String>,
    /// How old a request may be before it is refused.
    pub tolerance_seconds: i32,
    /// The secret store row holding the shared secret.
    pub secret_id: Option<uuid::Uuid>,
    /// The cap, applied before authentication.
    pub max_payload_bytes: i32,
    /// One of [`SANITIZE_PROFILES`].
    pub sanitize_profile: String,
    /// Whether the declaration is enforced.
    pub enabled: bool,
}

impl IntakeEndpoint {
    /// Reject a declaration the platform will not run, naming the field and the bound.
    pub fn validate(&self) -> Result<()> {
        if !self.path.starts_with('/') {
            return Err(ReliabilityError::invalid(format!(
                "path must start with '/', got '{}'",
                self.path
            )));
        }
        if self.name.trim().is_empty() {
            return Err(ReliabilityError::invalid("name must not be empty"));
        }
        if !INTAKE_SCHEMES.contains(&self.hmac_scheme.as_str()) {
            return Err(ReliabilityError::invalid(format!(
                "hmac_scheme must be one of {}, got '{}'",
                INTAKE_SCHEMES.join(", "),
                self.hmac_scheme
            )));
        }
        if self.signature_header.trim().is_empty() {
            return Err(ReliabilityError::invalid(
                "signature_header must not be empty",
            ));
        }
        if !(MIN_TOLERANCE_SECONDS..=MAX_TOLERANCE_SECONDS).contains(&self.tolerance_seconds) {
            return Err(ReliabilityError::invalid(format!(
                "tolerance_seconds must be between {MIN_TOLERANCE_SECONDS} and {MAX_TOLERANCE_SECONDS}, got {}",
                self.tolerance_seconds
            )));
        }
        if !(MIN_PAYLOAD_BYTES..=MAX_PAYLOAD_BYTES).contains(&self.max_payload_bytes) {
            return Err(ReliabilityError::invalid(format!(
                "max_payload_bytes must be between {MIN_PAYLOAD_BYTES} and {MAX_PAYLOAD_BYTES}, got {}",
                self.max_payload_bytes
            )));
        }
        if !SANITIZE_PROFILES.contains(&self.sanitize_profile.as_str()) {
            return Err(ReliabilityError::invalid(format!(
                "sanitize_profile must be one of {}, got '{}'",
                SANITIZE_PROFILES.join(", "),
                self.sanitize_profile
            )));
        }
        Ok(())
    }
}

/// What arrived, as the guard sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inbound {
    /// The endpoint's declared path.
    pub path: String,
    /// The declared cap, in bytes.
    pub max_payload_bytes: i32,
    /// The body's length in bytes.
    pub body_len: usize,
    /// The body's content type, if one was sent.
    pub content_type: Option<String>,
    /// The signature header's value, lower-cased key.
    pub signature: Option<String>,
    /// The timestamp header's value, as a unix second count.
    pub timestamp: Option<i64>,
    /// A signature id extracted from the signature (`v1,<id>:<tag>`), when the scheme has one.
    pub signature_id: Option<String>,
    /// Where the request came from.
    pub source_ip: Option<IpAddr>,
    /// The request id, for the rejection row.
    pub request_id: Option<uuid::Uuid>,
}

/// What the guard decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum GuardVerdict {
    /// The request is genuine and may be parsed.
    Accepted {
        /// The body with control characters removed, when the profile says to remove them.
        sanitized_body: String,
        /// What the pass changed, for the request log.
        changes: Vec<String>,
    },
    /// The request is refused, with the documented reason code.
    Rejected {
        /// One of [`INTAKE_REASONS`], and the same string the wire answer carries.
        reason: &'static str,
        /// A human sentence for the operator, carrying no payload and no secret.
        detail: String,
        /// Whether a rejection row must be written.
        record: bool,
    },
}

/// Run the guard over one inbound request.
///
/// The order is the design, and each step exists because the one after it is more expensive or
/// less safe than the one before:
///
/// 1. **Size** — the cheapest check, and the one that must come first so an oversized upload
///    costs a `413` rather than an HMAC over its whole body.
/// 2. **Content type** — a declared endpoint that only speaks JSON refuses a form post before
///    parsing anything.
/// 3. **Signature presence**, then **timestamp freshness**, then **replay**, then
///    **constant-time comparison** — in that order, so the cheapest reason for a rejection is
///    the one reported. A request with a stale timestamp and a bad signature is reported as
///    stale, which is the more actionable of the two.
/// 4. **Sanitisation** of what survived, never of what was refused.
#[must_use]
pub fn evaluate(
    endpoint: &IntakeEndpoint,
    inbound: &Inbound,
    body: &str,
    secret: &[u8],
    now: OffsetDateTime,
    seen_ids: &BTreeSet<String>,
) -> GuardVerdict {
    if !endpoint.enabled {
        return GuardVerdict::Rejected {
            reason: "malformed",
            detail: "this endpoint is declared but not enabled".into(),
            record: false,
        };
    }
    if inbound.body_len > endpoint.max_payload_bytes as usize {
        return GuardVerdict::Rejected {
            reason: "payload_too_large",
            detail: format!(
                "body is {} bytes, the cap for this endpoint is {}",
                inbound.body_len, endpoint.max_payload_bytes
            ),
            record: true,
        };
    }
    if let Some(content_type) = &inbound.content_type {
        let ok = matches!(
            content_type.split(';').next().unwrap_or("").trim(),
            "application/json" | "application/x-www-form-urlencoded" | "text/plain"
        );
        if !ok {
            return GuardVerdict::Rejected {
                reason: "content_type_refused",
                detail: format!("content type '{content_type}' is not accepted here"),
                record: true,
            };
        }
    }
    let Some(signature) = &inbound.signature else {
        return GuardVerdict::Rejected {
            reason: "signature_missing",
            detail: format!("no '{}' header was sent", endpoint.signature_header),
            record: true,
        };
    };
    if let Some(ts) = inbound.timestamp {
        let age = (now.unix_timestamp() - ts).abs();
        if age > i64::from(endpoint.tolerance_seconds) {
            return GuardVerdict::Rejected {
                reason: "timestamp_stale",
                detail: format!(
                    "the request timestamp is {age}s old, the tolerance here is {}s",
                    endpoint.tolerance_seconds
                ),
                record: true,
            };
        }
    }
    if let Some(id) = &inbound.signature_id {
        if seen_ids.contains(id) {
            return GuardVerdict::Rejected {
                reason: "replay",
                detail: "this signature id has already been used inside the tolerance window"
                    .into(),
                record: true,
            };
        }
    }
    if !verify(endpoint, body, secret, signature) {
        return GuardVerdict::Rejected {
            reason: "signature_invalid",
            detail: "the signature does not match the body".into(),
            record: true,
        };
    }
    let (sanitized_body, changes) = sanitize(body, &endpoint.sanitize_profile);
    GuardVerdict::Accepted {
        sanitized_body,
        changes,
    }
}

/// Verify a signature in constant time.
///
/// The `v1,<id>:<tag>` shape is the one every major provider uses, so it is the one this
/// accepts; a bare hex tag is also accepted because a hand-rolled integration writes that and
/// refusing it teaches the operator nothing about what went wrong. The comparison is
/// `ConstantTimeEq` over the decoded bytes — comparing hex **strings** would be constant time in
/// the length only, and two tags of different length would short-circuit on length alone.
#[must_use]
pub fn verify(endpoint: &IntakeEndpoint, body: &str, secret: &[u8], signature: &str) -> bool {
    let presented = match decode_signature(endpoint, signature) {
        Some(tag) => tag,
        None => return false,
    };
    let expected = compute_tag(endpoint, body, secret);
    presented.len() == expected.len() && bool::from(presented.ct_eq(&expected))
}

/// Extract the raw tag bytes from a presented signature.
fn decode_signature(endpoint: &IntakeEndpoint, signature: &str) -> Option<Vec<u8>> {
    // `v1,<id>:<tag>` — the id is the replay defence, the tag is the proof.
    let tag_part = match signature.rsplit_once(':') {
        Some((prefix, tag)) if prefix.starts_with("v1,") || prefix == "v1" => tag,
        _ => signature,
    };
    match endpoint.hmac_scheme.as_str() {
        "sha256_base64" => {
            // Standard base64 with padding, then the URL-safe alphabet without it: both appear
            // in the wild and a provider that sends one of them is not misconfigured.
            let normalised = tag_part.replace('-', "+").replace('_', "/");
            decode_base64(&normalised)
        }
        _ => hex::decode(tag_part).ok(),
    }
}

/// The tag this body should produce under this scheme.
fn compute_tag(endpoint: &IntakeEndpoint, body: &str, secret: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(body.as_bytes());
    let digest = mac.finalize().into_bytes();
    match endpoint.hmac_scheme.as_str() {
        "sha1_hex" => hex::decode(&hex::encode(&digest)[..40]).unwrap_or_default(),
        _ => digest.to_vec(),
    }
}

/// Minimal standard base64 decode, so the crate carries no extra dependency for one alphabet.
fn decode_base64(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lookup = [255u8; 256];
    for (i, c) in TABLE.iter().enumerate() {
        lookup[*c as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer: u32 = 0;
    let mut bits = 0u32;
    for byte in input.bytes() {
        if byte == b'=' {
            break;
        }
        let value = lookup[byte as usize];
        if value == 255 {
            return None;
        }
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

/// How many bytes the character starting with this byte occupies, or 0 for a continuation.
///
/// UTF-8's lead bytes carry their own length, so the width is read rather than counted — a
/// counting loop would split a two-byte character in half and produce invalid output from a
/// function whose whole job is to keep output valid.
fn utf8_len(lead: u8) -> usize {
    match lead {
        0x00..=0x7F => 1,
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => 0,
    }
}

/// Remove control characters from a body, and report what changed.
///
/// **Narrow on purpose.** The request's own risk note is that a "helpful" rewriting guard
/// corrupts real payloads and is worse than no guard, so this removes C0 controls (and DEL) and
/// nothing else: no trimming of meaningful whitespace, no re-encoding, no Unicode normalisation.
/// Under `strict` the escapes are also stripped, which is the difference between "this string
/// contained a newline" and "this string contained a literal `\n` two characters" — the second
/// is almost always an injection attempt and almost never a real payload.
///
/// The test asserts a legitimate JSON payload comes back **byte-identical**, because a guard
/// that changes real payloads is the failure this profile exists to prevent.
#[must_use]
pub fn sanitize(body: &str, profile: &str) -> (String, Vec<String>) {
    let strict = profile == "strict";
    let mut out = String::with_capacity(body.len());
    let mut changes: Vec<String> = Vec::new();
    let mut controls = 0usize;
    let mut escapes = 0usize;
    // The input is walked as BYTES with an index, never through the output's length. The first
    // draft read `body.get(out.len()..)`, which is the output's cursor applied to the input —
    // correct only until the first control character is dropped, and the strict-escape test
    // failed for exactly that reason: with a NUL earlier in the body, every later `\uXXXX`
    // stopped being detected. A sanitiser whose own cursor desynchronises is a sanitiser that
    // silently stops sanitising.
    let bytes = body.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let ch_len = utf8_len(bytes[i]);
        if ch_len == 0 {
            // A stray continuation byte: not valid UTF-8, so it cannot be a character.
            controls += 1;
            i += 1;
            continue;
        }
        let slice = &body[i..i + ch_len];
        let ch = slice.chars().next().expect("length came from the first byte");
        let is_control =
            (ch.is_control() && ch != '\n' && ch != '\r' && ch != '\t') || ch == '\u{7f}';
        if is_control {
            controls += 1;
            i += ch_len;
            continue;
        }
        if strict && ch == '\\' {
            // Only a `\uXXXX` escape is touched. A lone backslash in a Windows path is data,
            // and a `\n` inside a JSON string is ordinary.
            let after = i + 1;
            if after + 5 <= bytes.len()
                && bytes[after] == b'u'
                && bytes[after + 1..after + 5].iter().all(u8::is_ascii_hexdigit)
            {
                escapes += 1;
                out.push('\u{FFFD}');
                i = after + 5;
                continue;
            }
        }
        out.push_str(slice);
        i += ch_len;
    }
    if controls > 0 {
        changes.push(format!("removed {controls} control character(s)"));
    }
    if escapes > 0 {
        changes.push(format!("replaced {escapes} unicode escape sequence(s)"));
    }
    (out, changes)
}

/// The row a rejection writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rejection {
    /// The endpoint, when the path matched a declaration.
    pub endpoint_id: Option<uuid::Uuid>,
    /// One of [`INTAKE_REASONS`].
    pub reason: String,
    /// Where it came from.
    pub source_ip: Option<IpAddr>,
    /// The request id, so the refusal is findable in the log explorer.
    pub request_id: Option<uuid::Uuid>,
    /// Never the payload. This field exists to make that structural.
    pub body_bytes: usize,
}

/// The event name a rejection emits.
#[must_use]
pub fn rejection_event() -> &'static str {
    crate::vocabulary::EVENT_NAMES[8] // reliability.intake.rejected
}

/// Verify an operator-supplied sample for the screen's `Verify sample` action.
///
/// Same function the request path uses — that is the whole point of the action. A tester with
/// its own signature check would answer "valid" for a body the platform would refuse, which is
/// the one answer an operator must never be given by a tool that claims to be the platform.
#[must_use]
pub fn verify_sample(
    endpoint: &IntakeEndpoint,
    sample: &str,
    signature: &str,
    secret: &[u8],
) -> GuardVerdict {
    let inbound = Inbound {
        path: endpoint.path.clone(),
        max_payload_bytes: endpoint.max_payload_bytes,
        body_len: sample.len(),
        content_type: Some("application/json".into()),
        signature: Some(signature.to_string()),
        // No timestamp: a sample is about the tag, and refusing it for a missing timestamp
        // would teach the operator nothing about the signature they are checking.
        timestamp: None,
        signature_id: None,
        source_ip: None,
        request_id: None,
    };
    evaluate(endpoint, &inbound, sample, secret, OffsetDateTime::UNIX_EPOCH, &BTreeSet::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn at(seconds: i64) -> OffsetDateTime {
        datetime!(2026-01-01 00:00 UTC) + time::Duration::seconds(seconds)
    }

    fn endpoint() -> IntakeEndpoint {
        IntakeEndpoint {
            path: "/api/v1/webhooks/demo".into(),
            name: "demo".into(),
            hmac_scheme: "sha256_hex".into(),
            signature_header: "x-demo-signature".into(),
            timestamp_header: Some("x-demo-timestamp".into()),
            tolerance_seconds: 300,
            secret_id: Some(uuid::Uuid::from_u128(1)),
            max_payload_bytes: 1_024,
            sanitize_profile: "balanced".into(),
            enabled: true,
        }
    }

    fn signed(endpoint: &IntakeEndpoint, body: &str, secret: &[u8]) -> String {
        format!("v1,abc123:{}", hex::encode(compute_tag(endpoint, body, secret)))
    }

    fn inbound(signature: Option<String>, body: &str) -> Inbound {
        Inbound {
            path: "/api/v1/webhooks/demo".into(),
            max_payload_bytes: 1_024,
            body_len: body.len(),
            content_type: Some("application/json".into()),
            signature,
            timestamp: Some(at(0).unix_timestamp()),
            signature_id: Some("abc123".into()),
            source_ip: None,
            request_id: None,
        }
    }

    fn reason(v: &GuardVerdict) -> &'static str {
        match v {
            GuardVerdict::Rejected { reason, .. } => reason,
            GuardVerdict::Accepted { .. } => "accepted",
        }
    }

    #[test]
    fn a_valid_signature_is_accepted() {
        let e = endpoint();
        let body = r#"{"event":"ping"}"#;
        let sig = signed(&e, body, b"shhh");
        let v = evaluate(
            &e,
            &inbound(Some(sig), body),
            body,
            b"shhh",
            at(0),
            &BTreeSet::new(),
        );
        assert_eq!(reason(&v), "accepted");
    }

    #[test]
    fn a_tampered_body_is_refused_with_signature_invalid() {
        let e = endpoint();
        let sig = signed(&e, r#"{"amount":1}"#, b"shhh");
        let v = evaluate(
            &e,
            &inbound(Some(sig), r#"{"amount":9999}"#),
            r#"{"amount":9999}"#,
            b"shhh",
            at(0),
            &BTreeSet::new(),
        );
        assert_eq!(reason(&v), "signature_invalid");
    }

    #[test]
    fn a_wrong_secret_is_refused() {
        let e = endpoint();
        let body = "{}";
        let sig = signed(&e, body, b"shhh");
        let v = evaluate(
            &e,
            &inbound(Some(sig), body),
            body,
            b"different",
            at(0),
            &BTreeSet::new(),
        );
        assert_eq!(reason(&v), "signature_invalid");
    }

    #[test]
    fn a_stale_timestamp_is_refused_and_named_as_such() {
        let e = endpoint();
        let body = "{}";
        let sig = signed(&e, body, b"shhh");
        let mut inb = inbound(Some(sig), body);
        inb.timestamp = Some(at(-3_600).unix_timestamp());
        let v = evaluate(&e, &inb, body, b"shhh", at(0), &BTreeSet::new());
        assert_eq!(reason(&v), "timestamp_stale");
    }

    #[test]
    fn a_replayed_signature_id_is_refused() {
        let e = endpoint();
        let body = "{}";
        let sig = signed(&e, body, b"shhh");
        let seen: BTreeSet<String> = ["abc123".to_string()].into_iter().collect();
        let v = evaluate(
            &e,
            &inbound(Some(sig), body),
            body,
            b"shhh",
            at(0),
            &seen,
        );
        assert_eq!(reason(&v), "replay");
    }

    #[test]
    fn a_missing_signature_is_refused_before_anything_is_hashed() {
        let e = endpoint();
        let v = evaluate(
            &e,
            &inbound(None, "{}"),
            "{}",
            b"shhh",
            at(0),
            &BTreeSet::new(),
        );
        assert_eq!(reason(&v), "signature_missing");
    }

    #[test]
    fn an_oversized_body_is_refused_with_413_payload_too_large_and_never_hashed() {
        let e = endpoint();
        let mut inb = inbound(Some("v1,x:00".into()), "{}");
        // 2000 bytes against a 1024 cap, with a signature that would not verify anyway.
        inb.body_len = 2_000;
        let v = evaluate(&e, &inb, "{}", b"shhh", at(0), &BTreeSet::new());
        // Size is checked FIRST, so the answer is `payload_too_large` and not
        // `signature_invalid` — a refusal that costs a `413` instead of an HMAC.
        assert_eq!(reason(&v), "payload_too_large");
    }

    #[test]
    fn an_unknown_content_type_is_refused() {
        let e = endpoint();
        let body = "{}";
        let sig = signed(&e, body, b"shhh");
        let mut inb = inbound(Some(sig), body);
        inb.content_type = Some("text/html".into());
        let v = evaluate(&e, &inb, body, b"shhh", at(0), &BTreeSet::new());
        assert_eq!(reason(&v), "content_type_refused");
    }

    #[test]
    fn a_content_type_with_a_charset_parameter_is_still_accepted() {
        let e = endpoint();
        let body = "{}";
        let sig = signed(&e, body, b"shhh");
        let mut inb = inbound(Some(sig), body);
        inb.content_type = Some("application/json; charset=utf-8".into());
        assert_eq!(
            reason(&evaluate(&e, &inb, body, b"shhh", at(0), &BTreeSet::new())),
            "accepted"
        );
    }

    #[test]
    fn a_bare_hex_tag_without_the_v1_prefix_is_accepted() {
        // A hand-rolled integration writes this, and refusing it teaches nothing.
        let e = endpoint();
        let body = "{}";
        let sig = hex::encode(compute_tag(&e, body, b"shhh"));
        let mut inb = inbound(None, body);
        inb.signature = Some(sig);
        inb.timestamp = Some(at(0).unix_timestamp());
        inb.signature_id = None;
        assert_eq!(
            reason(&evaluate(&e, &inb, body, b"shhh", at(0), &BTreeSet::new())),
            "accepted"
        );
    }

    #[test]
    fn the_base64_scheme_verifies_its_own_tag() {
        let mut e = endpoint();
        e.hmac_scheme = "sha256_base64".into();
        let body = r#"{"x":1}"#;
        let tag = compute_tag(&e, body, b"shhh");
        let b64 = encode_base64(&tag);
        let mut inb = inbound(Some(format!("v1,id:{b64}")), body);
        inb.signature_id = None;
        assert_eq!(
            reason(&evaluate(&e, &inb, body, b"shhh", at(0), &BTreeSet::new())),
            "accepted"
        );
        // And a wrong body is still refused through the same path.
        let mut bad = inb.clone();
        bad.signature = Some(format!("v1,id:{b64}"));
        assert_eq!(
            reason(&evaluate(&e, &bad, "{\"x\":2}", b"shhh", at(0), &BTreeSet::new())),
            "signature_invalid"
        );
    }

    /// Standard base64 encode, matching [`decode_base64`].
    fn encode_base64(data: &[u8]) -> String {
        const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            out.push(TABLE[(n >> 18) as usize & 63] as char);
            out.push(TABLE[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
            out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
        }
        out
    }

    #[test]
    fn sanitisation_removes_control_characters_and_reports_it() {
        let (out, changes) = sanitize("hello\u{0}world\u{7f}", "balanced");
        assert_eq!(out, "helloworld");
        assert!(changes.iter().any(|c| c.contains("2 control character")));
    }

    #[test]
    fn sanitisation_keeps_newlines_tabs_and_carriage_returns() {
        // A legitimate payload with a pretty-printed body must survive untouched.
        let body = "{\n  \"a\": 1,\r\n  \"b\": \"x\ty\"\n}";
        let (out, changes) = sanitize(body, "balanced");
        assert_eq!(out, body);
        assert!(changes.is_empty());
    }

    #[test]
    fn a_legitimate_json_payload_is_byte_identical_after_sanitisation() {
        // The acceptance line, and the reason the pass is narrow: a guard that rewrites real
        // payloads is worse than no guard.
        let body = r#"{"id":42,"name":"Furkan Ermağ","tags":["a","b"],"n":-1.5e10,"ok":true}"#;
        let (out, changes) = sanitize(body, "balanced");
        assert_eq!(out, body);
        assert!(changes.is_empty());
    }

    #[test]
    fn strict_profile_also_strips_unicode_escapes() {
        let (out, changes) = sanitize(r#"{"a":"\u003cscript\u003e"}"#, "strict");
        // One escape becomes ONE replacement character: the six source characters
        // `\u003c` decode to a single value, and emitting two would corrupt the JSON by
        // changing its length — the "helpful rewriting is worse than no guard" failure.
        assert_eq!(out, "{\"a\":\"\u{fffd}script\u{fffd}\"}");
        assert!(changes.iter().any(|c| c.contains("unicode escape")));
        // And balanced leaves them alone, because they are ordinary data there.
        let (out, _) = sanitize(r#"{"a":"\u003c"}"#, "balanced");
        assert_eq!(out, r#"{"a":"\u003c"}"#);
    }

    #[test]
    fn a_lone_backslash_is_not_an_escape() {
        let (out, changes) = sanitize(r"C:\Users\furkan\file.txt", "strict");
        assert_eq!(out, r"C:\Users\furkan\file.txt");
        assert!(changes.is_empty());
    }

    #[test]
    fn a_rejection_never_carries_the_payload() {
        let e = endpoint();
        let body = "{\"card\":\"4111111111111111\"}";
        let GuardVerdict::Rejected { detail, .. } =
            evaluate(&e, &inbound(None, body), body, b"shhh", at(0), &BTreeSet::new())
        else {
            panic!("expected a rejection");
        };
        assert!(!detail.contains("4111"), "the reason echoed the payload: {detail}");
    }

    #[test]
    fn the_sample_verifier_agrees_with_the_request_path() {
        let e = endpoint();
        let body = r#"{"event":"ping"}"#;
        let sig = signed(&e, body, b"shhh");
        assert_eq!(reason(&verify_sample(&e, body, &sig, b"shhh")), "accepted");
        // A wrong signature is refused, not accepted — the one answer the tool must never lie
        // about.
        assert_eq!(
            reason(&verify_sample(&e, body, "v1,id:00", b"shhh")),
            "signature_invalid"
        );
    }

    #[test]
    fn a_disabled_endpoint_refuses_without_recording() {
        let mut e = endpoint();
        e.enabled = false;
        let v = evaluate(&e, &inbound(None, "{}"), "{}", b"s", at(0), &BTreeSet::new());
        let GuardVerdict::Rejected { record, .. } = v else {
            panic!("expected a rejection");
        };
        assert!(!record, "a disabled declaration is not a rejection worth logging");
    }

    #[test]
    fn validation_names_the_field_and_the_bound() {
        let mut e = endpoint();
        e.path = "webhooks".into();
        assert!(e.validate().unwrap_err().to_string().contains("path"));
        let mut e = endpoint();
        e.hmac_scheme = "md5".into();
        assert!(e.validate().unwrap_err().to_string().contains("hmac_scheme"));
        let mut e = endpoint();
        e.tolerance_seconds = 5;
        assert!(e.validate().unwrap_err().to_string().contains("tolerance_seconds"));
        let mut e = endpoint();
        e.max_payload_bytes = 10;
        assert!(e.validate().unwrap_err().to_string().contains("max_payload_bytes"));
        let mut e = endpoint();
        e.sanitize_profile = "paranoid".into();
        assert!(e.validate().unwrap_err().to_string().contains("sanitize_profile"));
        assert!(endpoint().validate().is_ok());
    }

    #[test]
    fn every_reason_this_module_can_return_is_in_the_vocabulary() {
        for r in [
            "payload_too_large",
            "content_type_refused",
            "signature_missing",
            "timestamp_stale",
            "replay",
            "signature_invalid",
            "malformed",
        ] {
            assert!(INTAKE_REASONS.contains(&r), "{r} is not a stored reason");
        }
        // And each is also the wire code, not a translation of one.
        assert!(INTAKE_REASONS.contains(&"payload_too_large"));
    }

    #[test]
    fn every_scheme_the_module_accepts_is_declared() {
        for s in ["sha256_hex", "sha256_base64", "sha1_hex"] {
            assert!(INTAKE_SCHEMES.contains(&s), "{s} is accepted but not declared");
        }
    }
}
