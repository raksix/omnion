//! The visitor identifier and the address rules — the two places the privacy promise lives.
//!
//! Nothing here stores a person: the identifier is a SHA-256 of `(site, day-salt, address,
//! user agent)`, the salt is rotated at midnight and pruned with the raw data, and an address
//! is either not written at all (the default) or truncated to its network (`/24` for IPv4,
//! `/48` for IPv6). Both facts are enforced in code *and* in the schema (see
//! `database/migrations/0015_analytics.sql`), so a later reader cannot change one and miss the
//! other.

use std::net::IpAddr;

use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::Date;
use uuid::Uuid;

use crate::error::Result;

/// Generate the salt of one day: 32 random bytes, hex-encoded (the schema checks the shape).
#[must_use]
pub fn new_salt() -> String {
    let mut bytes = [0_u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// The salt of `day`, created on first use — the collector may be the first thing to run that
/// day, and a missing salt must never be a reason to skip a beacon.
///
/// Two instances racing on the same first request both insert; the loser's candidate is
/// discarded by `on conflict do nothing` and the stored salt is the one that answers, so the
/// same visitor hashes the same way on every instance.
pub async fn daily_salt(pool: &PgPool, day: Date) -> Result<String> {
    sqlx::query(
        "insert into analytics_salts (day, salt) values ($1, $2) on conflict (day) do nothing",
    )
    .bind(day)
    .bind(new_salt())
    .execute(pool)
    .await?;

    let salt: String = sqlx::query_scalar("select salt from analytics_salts where day = $1")
        .bind(day)
        .fetch_one(pool)
        .await?;

    Ok(salt)
}

/// The visitor identifier of one beacon: a hash of the site, the day's salt, the address and
/// the user agent.
///
/// Missing pieces hash as empty strings on purpose: two visitors behind a proxy that strips
/// addresses are one visitor for the day, which is the conservative direction (fewer visitors,
/// never more).
#[must_use]
pub fn hash(site_id: Uuid, salt: &str, address: Option<IpAddr>, user_agent: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(site_id.as_bytes());
    hasher.update(b"|");
    hasher.update(salt.as_bytes());
    hasher.update(b"|");
    hasher.update(address.map(|ip| ip.to_string()).unwrap_or_default());
    hasher.update(b"|");
    hasher.update(user_agent.trim().as_bytes());
    hex::encode(hasher.finalize())
}

/// Truncate an address to its network: `203.0.113.7` → `203.0.113.0/24`, and the first 48 bits
/// of an IPv6 address. The full address never leaves this function.
#[must_use]
pub fn truncate(address: IpAddr) -> String {
    match address {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            format!("{}.{}.{}.0/24", octets[0], octets[1], octets[2])
        }
        IpAddr::V6(v6) => {
            let segments = v6.segments();
            format!("{:x}:{:x}:{:x}::/48", segments[0], segments[1], segments[2])
        }
    }
}

/// One entry of an exclusion list: a single address or a network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpRule {
    /// A single address (`203.0.113.7`).
    Address(IpAddr),
    /// A network (`203.0.113.0/24`, `2001:db8::/32`).
    Network(IpAddr, u8),
}

impl IpRule {
    /// Parse one line of the exclusion list; `None` means the operator made a typo, which the
    /// settings validation reports rather than silently ignoring.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }

        match text.split_once('/') {
            None => text.parse::<IpAddr>().ok().map(Self::Address),
            Some((address, prefix)) => {
                let address = address.trim().parse::<IpAddr>().ok()?;
                let prefix = prefix.trim().parse::<u8>().ok()?;
                let max = if address.is_ipv4() { 32 } else { 128 };
                if prefix > max {
                    return None;
                }
                Some(Self::Network(address, prefix))
            }
        }
    }

    /// `true` when `address` is covered by this rule.
    #[must_use]
    pub fn contains(&self, address: IpAddr) -> bool {
        match *self {
            Self::Address(own) => own == address,
            Self::Network(network, prefix) => within(network, prefix, address),
        }
    }
}

/// `true` when `address` shares the first `prefix` bits with `network`.
fn within(network: IpAddr, prefix: u8, address: IpAddr) -> bool {
    match (network, address) {
        (IpAddr::V4(network), IpAddr::V4(address)) => {
            let bits = prefix.min(32);
            if bits == 0 {
                return true;
            }
            let shift = 32 - bits;
            (u32::from(network) >> shift) == (u32::from(address) >> shift)
        }
        (IpAddr::V6(network), IpAddr::V6(address)) => {
            let bits = prefix.min(128);
            if bits == 0 {
                return true;
            }
            let shift = 128 - bits;
            (u128::from(network) >> shift) == (u128::from(address) >> shift)
        }
        _ => false,
    }
}

/// Match a path against one exclusion pattern.
///
/// A pattern is either an exact path (`/checkout`), a prefix (`/admin/*`) or a suffix
/// (`*.pdf`) — the whole grammar, because a wildcard language in a settings field is a
/// liability, not a feature.
#[must_use]
pub fn path_matches(pattern: &str, path: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        return path.starts_with(prefix);
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return path.ends_with(suffix);
    }
    pattern == path
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn the_salt_is_daily_and_well_formed() {
        let salt = new_salt();
        assert_eq!(salt.len(), 64);
        assert!(salt.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(salt, new_salt());
    }

    #[test]
    fn the_same_pieces_hash_the_same_and_a_new_salt_does_not() {
        let site = Uuid::nil();
        let address = Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)));
        let first = hash(site, "a", address, "Mozilla/5.0");
        assert_eq!(first, hash(site, "a", address, "Mozilla/5.0"));
        assert_ne!(first, hash(site, "b", address, "Mozilla/5.0"));
        assert_ne!(first, hash(Uuid::new_v4(), "a", address, "Mozilla/5.0"));
        assert_ne!(
            first,
            hash(
                site,
                "a",
                Some(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9))),
                "Mozilla/5.0"
            )
        );
        assert_eq!(first.len(), 64);
    }

    #[test]
    fn addresses_are_truncated_to_their_network_and_never_stored_whole() {
        assert_eq!(
            truncate(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7))),
            "203.0.113.0/24"
        );
        let v6: IpAddr = "2001:db8:1:2:3:4:5:6".parse().unwrap();
        assert_eq!(truncate(v6), "2001:db8:1::/48");
    }

    #[test]
    fn exclusion_rules_parse_and_match_the_family_they_belong_to() {
        let single = IpRule::parse("203.0.113.7").expect("a single address parses");
        assert!(single.contains("203.0.113.7".parse().unwrap()));
        assert!(!single.contains("203.0.113.8".parse().unwrap()));

        let network = IpRule::parse("203.0.113.0/24").expect("a network parses");
        assert!(network.contains("203.0.113.9".parse().unwrap()));
        assert!(!network.contains("203.0.114.1".parse().unwrap()));

        let wide = IpRule::parse("2001:db8::/32").expect("an IPv6 network parses");
        assert!(wide.contains("2001:db8:1::9".parse().unwrap()));
        assert!(!wide.contains("2001:db9::1".parse().unwrap()));
        assert!(
            !wide.contains("203.0.113.7".parse().unwrap()),
            "families never mix"
        );

        assert!(IpRule::parse("not-an-address").is_none());
        assert!(
            IpRule::parse("203.0.113.0/33").is_none(),
            "a prefix above 32 is a typo"
        );
        assert!(IpRule::parse("").is_none());
    }

    #[test]
    fn path_patterns_are_exact_prefix_or_suffix() {
        assert!(path_matches("/checkout", "/checkout"));
        assert!(!path_matches("/checkout", "/checkout/done"));
        assert!(path_matches("/admin/*", "/admin/users"));
        assert!(!path_matches("/admin/*", "/administrator"));
        assert!(path_matches("*.pdf", "/files/report.pdf"));
        assert!(!path_matches("*.pdf", "/files/report.pdf?x=1"));
    }
}
