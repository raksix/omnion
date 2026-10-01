//! Turning a presented bearer token into an authenticated principal (REQ-033, slice 1).
//!
//! This module is the *consumer* half of [`crate::secret`], and it is deliberately a separate
//! type from the panel's session so that a machine call and a signed-in person cannot be
//! mistaken for one another — the same reason `ApiCaller` is an enum rather than a struct with
//! an optional id.
//!
//! # The order of the checks, and why it is that order
//!
//! A key arrives as `omn_<prefix>.<secret>` with no cookie, so nothing about the request has
//! established *who* is calling. The checks run cheapest-and-most-selective first, and each one
//! that fails produces **the same refusal**:
//!
//! 1. **Shape.** [`secret::split_token`] — a free operation, and a malformed token must not
//!    cost a database probe. The answer is deliberately not "that is not a valid key": the
//!    format is not a secret, but telling a prober exactly which parts were wrong turns the
//!    format into an oracle for the rest.
//! 2. **Existence.** One index probe on `api_keys.prefix`. The hot path is a single lookup by
//!    a 48-bit public identifier, never a scan — which is what the two-halves design in
//!    [`secret`] is for.
//! 3. **Secret.** Constant-time comparison against the stored hash. A row this build cannot
//!    read ([`secret::is_readable_hash`]) fails here, not earlier, so an unreadable row is
//!    indistinguishable from a wrong secret.
//! 4. **State.** Revoked and expired keys are refused — and refused *with a reason*, because
//!    the person holding the key is the one who needs to know whether to rotate it or ask for
//!    an extension. This is the one refusal that says more than "invalid": it names the state
//!    and never the key, so it helps the legitimate holder without telling an attacker which
//!    of the two conditions a *valid* secret currently fails. See [`KeyRefusal`].
//! 5. **Source address.** The allowlist is checked last, after the secret has verified, so an
//!    allowlist cannot be used to distinguish "this key exists" from "this key does not": an
//!    address outside the list gets the same answer a wrong secret gets.
//!
//! # What a caller gets
//!
//! [`AuthenticatedKey`] carries the key's own scopes, not the owner's. That is the whole
//! scope-enforcement story for this slice: the API's own guard resolves a permission against
//! the organization, and this module decides whether the key *holds* it — see
//! [`AuthenticatedKey::allows`].

use std::net::IpAddr;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::DeveloperError;
use crate::model::ApiKey;
use crate::secret;

/// What a caller presented a key and the platform has decided about it.
///
/// A distinct type rather than a bare `bool` so that "no such key" and "wrong secret" cannot be
/// returned as two different things to a caller who then has to decide what to tell the client.
/// [`KeyRefusal::into_error`] is the only way this becomes an [`ApiError`]-shaped message,
/// and it is what guarantees the two stay one answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyRefusal {
    /// The token was malformed, unknown, or its secret did not match. One answer for all
    /// three: a caller who can tell "no such prefix" from "wrong secret" can enumerate which
    /// prefixes exist, which is a slow oracle and still an oracle.
    Invalid,
    /// The secret verified but the key has been withdrawn.
    Revoked,
    /// The secret verified but the key is past its expiry.
    Expired,
    /// The secret verified, the key is live, and the caller's address is not on its allowlist.
    ///
    /// Refused as [`KeyRefusal::Invalid`] rather than as its own case: an allowlist is meant to
    /// stop a *stolen* key, and a key that is stolen but not yet used from a blocked address
    /// must not be distinguishable from a key nobody has stolen. The legitimate holder sees
    /// `403` and the panel names the allowlist next to the key, which is where the answer
    /// belongs.
    AddressNotAllowed,
}

impl KeyRefusal {
    /// The message a caller receives.
    ///
    /// Two strings, and the split between them is the design: `Invalid` says only that the
    /// key is not usable, while `Revoked`/`Expired` name the state so the holder knows what to
    /// do. **None of them echoes the key**, so a `401` body and a proxy log line are both safe
    /// to paste into a support conversation.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::Invalid | Self::AddressNotAllowed => {
                "this API key is not valid for this request"
            }
            Self::Revoked => "this API key has been revoked",
            Self::Expired => "this API key has expired",
        }
    }

    /// Whether the refusal is a `401` (the credential is the problem) or a `403` (the credential
    /// is fine and the request is not permitted).
    ///
    /// Only the state refusals are `401`s that say *which* state, and they are still `401`:
    /// the caller must re-authenticate, and there is no credential they could add that would
    /// make this request succeed.
    #[must_use]
    pub fn status(self) -> u16 {
        match self {
            Self::Revoked | Self::Expired | Self::Invalid | Self::AddressNotAllowed => 401,
        }
    }

    /// Turn a refusal into the crate's own error, carrying the state through.
    #[must_use]
    pub fn into_error(self) -> DeveloperError {
        match self {
            Self::Revoked => DeveloperError::KeyNotActive("revoked"),
            Self::Expired => DeveloperError::KeyNotActive("expired"),
            // An address that is not allowed is answered as an invalid key, and so is a key that
            // never existed — deliberately indistinguishable.
            Self::Invalid | Self::AddressNotAllowed => DeveloperError::InvalidKey,
        }
    }
}

/// A key that has authenticated a request.
#[derive(Debug, Clone)]
pub struct AuthenticatedKey {
    /// The row, minus its secret: there is nothing to give back here even in memory.
    pub key: ApiKey,
    /// The organization the key speaks for.
    pub organization_id: Uuid,
}

impl AuthenticatedKey {
    /// Whether this key's own scopes include `permission`.
    ///
    /// Exact match, and that is the decision worth arguing about. A wildcard (`*`, or
    /// `developer.*`) would be a convenience that turns a key's scope list into decoration: a
    /// reviewer reading `omn_abc.def` sees a list of permissions and reasonably concludes those
    /// are the powers. An exact list is longer and cannot mislead. The one concession is the
    /// global `*`, which is explicit enough to be obvious in a list of twenty entries.
    #[must_use]
    pub fn allows(&self, permission: &str) -> bool {
        self.key.scopes.iter().any(|scope| scope == permission || scope == "*")
    }

    /// The scopes, joined, for a log line or an error detail.
    ///
    /// Scopes are permission *names*, never key material, so this is safe to print — and it is
    /// what a `403` body carries so a caller can see what its key actually holds.
    #[must_use]
    pub fn scope_list(&self) -> String {
        if self.key.scopes.is_empty() {
            return "none".to_owned();
        }
        self.key.scopes.join(", ")
    }
}

/// Whether `address` is inside any of a key's allowlisted CIDR blocks.
///
/// `None` (no allowlist) allows everything — that is the meaning of the column, and the form
/// expresses "no restriction" by omitting the field.
///
/// A block that does not parse is *skipped*, not treated as a match: an allowlist is a
/// restriction, so an entry we cannot read must not become a restriction nobody asked for (a
/// lockout with no explanation) — but it must not become a bypass either, and skipping it does
/// not: the address still has to match one of the entries that *did* parse.
#[must_use]
pub fn address_allowed(allowlist: &Option<Vec<String>>, address: Option<IpAddr>) -> bool {
    let Some(entries) = allowlist.as_ref() else {
        return true;
    };
    // An allowlist cannot be evaluated without an address, and refusing is the safe reading: a
    // deployment that strips `ConnectInfo` (the in-process test harnesses do) must not silently
    // lift every restriction. The panel shows the allowlist next to the key, so the answer is
    // findable.
    let Some(address) = address else {
        return false;
    };
    entries.iter().any(|entry| cidr_contains(entry, address))
}

/// Whether one CIDR block contains an address.
///
/// Parsed with `IpAddr` plus a prefix length rather than a netmask type, for the reason
/// `omnion_developer::store::validate_ip_allowlist` states: the platform validates the *shape*
/// at the edge and stores the strings, and this is the one place that has to mean something by
/// them. A block that does not parse is false.
#[must_use]
pub fn cidr_contains(entry: &str, address: IpAddr) -> bool {
    let Some((network, prefix)) = entry.trim().split_once('/') else {
        return false;
    };
    let Ok(prefix) = prefix.trim().parse::<u8>() else {
        return false;
    };
    let Ok(network) = network.trim().parse::<IpAddr>() else {
        return false;
    };
    match (network, address) {
        (IpAddr::V4(network), IpAddr::V4(address)) => {
            if prefix > 32 {
                return false;
            }
            // A `/0` block is a shift of 32, which would overflow a `u32`; the mask is computed
            // as a `u64` so the full range is representable without a special case.
            let mask = if prefix == 0 {
                0u64
            } else {
                u64::from(u32::MAX) << (32 - prefix)
            };
            (u64::from(u32::from(address)) & mask) == (u64::from(u32::from(network)) & mask)
        }
        (IpAddr::V6(network), IpAddr::V6(address)) => {
            if prefix > 128 {
                return false;
            }
            let mask = if prefix == 0 {
                0u128
            } else {
                u128::from(u128::MAX) << (128 - prefix)
            };
            (u128::from(address) & mask) == (u128::from(network) & mask)
        }
        // A v4 address is never inside a v6 block and the reverse is true, even where the v6
        // block is `::/0` — which would otherwise read as "every address" and turn a
        // well-intentioned `::/0` into a universal bypass.
        _ => false,
    }
}

/// The decision, given a resolved row and the shape checks already passed.
///
/// Split out from the database read so it can be tested exhaustively without one: the whole of
/// the interesting behaviour is the *order* and the *sameness* of the refusals, and neither is
/// observable through a `PgPool`.
#[must_use]
pub fn decide(
    key: &ApiKey,
    stored_hash: &str,
    secret_half: &str,
    address: Option<IpAddr>,
    now: OffsetDateTime,
) -> std::result::Result<(), KeyRefusal> {
    if !secret::verify(secret_half, stored_hash) {
        return Err(KeyRefusal::Invalid);
    }
    // Only *after* the secret verifies, so a revoked key does not answer faster than a live one
    // and the two cannot be told apart by a stopwatch on a wrong secret.
    match key.status_at(now) {
        crate::model::KeyStatus::Revoked => return Err(KeyRefusal::Revoked),
        crate::model::KeyStatus::Expired => return Err(KeyRefusal::Expired),
        _ => {}
    }
    if !address_allowed(&key.ip_allowlist, address) {
        return Err(KeyRefusal::AddressNotAllowed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Environment, KeyStatus, RateTier};
    use time::macros::datetime;

    fn key_at(expiry: Option<OffsetDateTime>, revoked: Option<OffsetDateTime>) -> ApiKey {
        ApiKey {
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            name: "test".to_owned(),
            prefix: "omn_000000000000".to_owned(),
            scopes: vec!["developer.keys.read".to_owned()],
            environment: Environment::Sandbox,
            rate_tier: RateTier::Standard,
            ip_allowlist: None,
            expires_at: expiry,
            last_used_at: None,
            revoked_at: revoked,
            rotated_at: None,
            created_by: Uuid::nil(),
            created_at: datetime!(2026-01-01 00:00 UTC),
            status: KeyStatus::Active,
        }
    }

    fn stored_for(secret_half: &str) -> String {
        secret::hash(secret_half)
    }

    #[test]
    fn a_correct_secret_from_an_allowed_address_is_accepted() {
        let key = key_at(None, None);
        let now = datetime!(2026-10-01 12:00 UTC);
        assert!(decide(&key, &stored_for("abc"), "abc", None, now).is_ok());
    }

    #[test]
    fn a_wrong_secret_is_refused_before_anything_else_is_looked_at() {
        // A revoked key presented with a wrong secret says "invalid", not "revoked": the state
        // is information about a key the caller has not proven they hold.
        let key = key_at(None, Some(datetime!(2026-09-01 00:00 UTC)));
        let now = datetime!(2026-10-01 12:00 UTC);
        let refusal = decide(&key, &stored_for("right"), "wrong", None, now).unwrap_err();
        assert_eq!(refusal, KeyRefusal::Invalid);
    }

    #[test]
    fn an_expired_key_with_the_right_secret_is_told_it_expired() {
        let key = key_at(Some(datetime!(2026-09-01 00:00 UTC)), None);
        let now = datetime!(2026-10-01 12:00 UTC);
        let refusal = decide(&key, &stored_for("right"), "right", None, now).unwrap_err();
        assert_eq!(refusal, KeyRefusal::Expired);
        assert_eq!(refusal.message(), "this API key has expired");
    }

    #[test]
    fn a_revoked_key_with_the_right_secret_is_told_it_was_revoked() {
        let key = key_at(None, Some(datetime!(2026-09-01 00:00 UTC)));
        let now = datetime!(2026-10-01 12:00 UTC);
        let refusal = decide(&key, &stored_for("right"), "right", None, now).unwrap_err();
        assert_eq!(refusal, KeyRefusal::Revoked);
    }

    #[test]
    fn an_address_outside_the_allowlist_is_answered_exactly_as_a_wrong_secret() {
        // The reason `AddressNotAllowed` collapses into `Invalid` at the *error* boundary and
        // not inside `decide`: an allowlist that produced its own response code would tell an
        // attacker which key is real. But an operator whose key suddenly stops working needs to
        // be told "your allowlist excluded you" rather than "your secret is wrong" — so the
        // distinction is preserved where a log line can read it and removed where a caller can.
        // Both halves are asserted, because a fix to either one alone is a regression of the
        // other.
        let mut key = key_at(None, None);
        key.ip_allowlist = Some(vec!["10.0.0.0/8".to_owned()]);
        let now = datetime!(2026-10-01 12:00 UTC);

        let outside = decide(
            &key,
            &stored_for("right"),
            "right",
            Some("203.0.113.9".parse().expect("a literal")),
            now,
        )
        .unwrap_err();
        // Internal: precise, so an operator reading the log is told the actual reason.
        assert_eq!(outside, KeyRefusal::AddressNotAllowed);
        // External: indistinguishable from a key that never existed. Asserted on the message
        // rather than on the error variant because the message is the contract a caller
        // actually sees — two variants rendering one string would pass, two strings would not.
        assert_eq!(
            outside.into_error().to_string(),
            DeveloperError::InvalidKey.to_string()
        );

        // And the same string a wrong secret produces.
        let wrong = decide(&key, &stored_for("right"), "wrong", None, now).unwrap_err();
        assert_eq!(wrong, KeyRefusal::Invalid);
        assert_eq!(wrong.message(), outside.message());

        let inside = decide(
            &key,
            &stored_for("right"),
            "right",
            Some("10.1.2.3".parse().expect("a literal")),
            now,
        );
        assert!(inside.is_ok());
    }

    #[test]
    fn an_allowlist_without_a_known_address_refuses_rather_than_lifting_the_restriction() {
        // `ConnectInfo` is absent in the in-process harnesses. Lifting the restriction there
        // would mean the rule is only enforced in production, which is the definition of a rule
        // that cannot be tested.
        let mut key = key_at(None, None);
        key.ip_allowlist = Some(vec!["10.0.0.0/8".to_owned()]);
        let now = datetime!(2026-10-01 12:00 UTC);
        let refusal = decide(&key, &stored_for("right"), "right", None, now).unwrap_err();
        assert_eq!(refusal, KeyRefusal::AddressNotAllowed);
        assert_eq!(
            refusal.into_error().to_string(),
            DeveloperError::InvalidKey.to_string()
        );
    }

    #[test]
    fn an_allowlist_reads_as_no_restriction_when_it_is_absent() {
        assert!(address_allowed(&None, None));
        assert!(address_allowed(&None, Some("203.0.113.9".parse().expect("a literal"))));
    }

    #[test]
    fn an_unparseable_allowlist_entry_matches_nothing_rather_than_everything() {
        // Skipping an unreadable entry must not become a bypass: the address still has to
        // match an entry that parsed.
        let list = Some(vec!["not-a-cidr".to_owned(), "10.0.0.0/8".to_owned()]);
        assert!(address_allowed(&list, Some("10.9.9.9".parse().expect("a literal"))));
        assert!(!address_allowed(&list, Some("203.0.113.9".parse().expect("a literal"))));

        let all_broken = Some(vec!["garbage".to_owned()]);
        assert!(!address_allowed(&all_broken, Some("10.0.0.1".parse().expect("a literal"))));
    }

    #[test]
    fn a_zero_length_block_contains_the_whole_family() {
        // `/0` is the shift-by-width case: `u32::MAX << 32` overflows in the obvious
        // formulation, and an allowlist of `0.0.0.0/0` is a legitimate "any IPv4".
        assert!(cidr_contains("0.0.0.0/0", "203.0.113.9".parse().expect("a literal")));
        assert!(cidr_contains("::/0", "2001:db8::1".parse().expect("a literal")));
        assert!(cidr_contains("10.0.0.0/8", "10.255.255.255".parse().expect("a literal")));
        assert!(!cidr_contains("10.0.0.0/8", "11.0.0.1".parse().expect("a literal")));
        // A single host.
        assert!(cidr_contains("192.168.1.1/32", "192.168.1.1".parse().expect("a literal")));
        assert!(!cidr_contains("192.168.1.1/32", "192.168.1.2".parse().expect("a literal")));
    }

    #[test]
    fn a_v6_block_never_contains_a_v4_address_even_at_slash_zero() {
        // `::/0` covers every *v6* address. Read as "everything" it would turn a
        // well-intentioned dual-stack allowlist into a universal bypass.
        assert!(!cidr_contains("::/0", "203.0.113.9".parse().expect("a literal")));
        assert!(!cidr_contains("0.0.0.0/0", "2001:db8::1".parse().expect("a literal")));
        assert!(cidr_contains("2001:db8::/32", "2001:db8:dead::1".parse().expect("a literal")));
        assert!(!cidr_contains("2001:db8::/32", "2001:db9::1".parse().expect("a literal")));
    }

    #[test]
    fn an_out_of_range_or_malformed_block_matches_nothing() {
        for bad in [
            "10.0.0.0/33",
            "10.0.0.0",
            "10.0.0.0/abc",
            "not-an-address/8",
            "2001:db8::/129",
            "",
        ] {
            assert!(
                !cidr_contains(bad, "10.0.0.1".parse().expect("a literal")),
                "{bad} should match nothing"
            );
        }
    }

    #[test]
    fn scopes_are_matched_exactly_so_a_review_of_the_list_is_a_review_of_the_power() {
        let key = AuthenticatedKey {
            key: ApiKey {
                scopes: vec![
                    "developer.keys.read".to_owned(),
                    "content.pages.read".to_owned(),
                ],
                ..key_at(None, None)
            },
            organization_id: Uuid::nil(),
        };
        assert!(key.allows("developer.keys.read"));
        assert!(key.allows("content.pages.read"));
        // The two refusals that matter: a prefix match would grant the write to a read-only key,
        // and an implicit suffix match would grant it to anything with a `developer` scope.
        assert!(!key.allows("developer.keys.manage"));
        assert!(!key.allows("developer.*"));
        assert!(!key.allows("keys.read"));
        assert!(!key.allows(""));
    }

    #[test]
    fn the_global_star_grants_exactly_one_key_and_is_visible_in_the_list() {
        let key = AuthenticatedKey {
            key: ApiKey { scopes: vec!["*".to_owned()], ..key_at(None, None) },
            organization_id: Uuid::nil(),
        };
        assert!(key.allows("anything.at.all"));
        assert_eq!(key.scope_list(), "*");
    }

    #[test]
    fn a_key_with_no_scopes_says_so_rather_than_rendering_an_empty_string() {
        let key = AuthenticatedKey {
            key: ApiKey { scopes: Vec::new(), ..key_at(None, None) },
            organization_id: Uuid::nil(),
        };
        assert_eq!(key.scope_list(), "none");
        assert!(!key.allows("developer.keys.read"));
    }

    #[test]
    fn no_refusal_message_or_status_echoes_the_key() {
        // The reason the state refusals exist at all: a `401` that says *which* state is the one
        // message a support conversation can carry without a redaction pass.
        for refusal in [
            KeyRefusal::Invalid,
            KeyRefusal::Revoked,
            KeyRefusal::Expired,
            KeyRefusal::AddressNotAllowed,
        ] {
            let message = refusal.message();
            assert!(!message.contains("omn_"), "message leaked a prefix: {message}");
            assert_eq!(refusal.status(), 401);
            assert!(!refusal.into_error().to_string().contains("omn_"));
        }
    }

    #[test]
    fn an_unreadable_stored_hash_is_an_invalid_key_rather_than_an_error() {
        // A row written by another build authenticates nobody. If this were an error rather
        // than a refusal it would be a 500, which tells the caller their key *exists* and this
        // server cannot read it.
        let key = key_at(None, None);
        let now = datetime!(2026-10-01 12:00 UTC);
        assert_eq!(
            decide(&key, "$argon2id$whatever", "right", None, now).unwrap_err(),
            KeyRefusal::Invalid
        );
    }
}
