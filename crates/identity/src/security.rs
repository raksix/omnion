//! The security policy: one document per organization that every sign-in path reads.
//!
//! The row carries the password rules, the lockout thresholds, the IP lists, the session
//! lifetimes and the device trust window (docs/07-IAM.md §12, table `security_policies`). The
//! database holds the authoritative range checks — this module holds the same ranges so a bad
//! value is refused as a field-level message before a statement runs, and so the panel can
//! explain the limit without a round trip.
//!
//! Nothing here decides HTTP: the API layer maps [`IdentityError::InvalidPolicy`] onto a `400`
//! whose `details.field` names the control the reader has to fix.

use std::net::IpAddr;

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{IdentityError, Result};

/// Idle lifetime assumed for an account without an organization (matches the table default).
pub const DEFAULT_IDLE_MINUTES: i32 = 120;

/// Absolute lifetime assumed without an organization (matches the table default).
pub const DEFAULT_ABSOLUTE_DAYS: i32 = 30;

/// Concurrent-session cap assumed without an organization (matches the table default).
pub const DEFAULT_CONCURRENT_MAX: i32 = 10;

/// One organization's security policy.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct SecurityPolicy {
    /// The organization the document belongs to.
    pub organization_id: Uuid,
    /// Minimum password length (`8–128`).
    pub password_min_length: i32,
    /// How many character classes a password must use (`1–4`).
    pub password_require_classes: i32,
    /// How many previous passwords are remembered (`0–24`).
    pub password_history: i32,
    /// Password expiry in days (`0` = never).
    pub password_expiry_days: i32,
    /// Failed attempts before an account locks (`3–50`).
    pub lockout_attempts: i32,
    /// How long a lockout lasts, in minutes (`1–1440`).
    pub lockout_minutes: i32,
    /// Networks allowed to sign in; empty means "any address".
    pub ip_allowlist: Vec<String>,
    /// Networks refused outright; deny wins over allow.
    pub ip_denylist: Vec<String>,
    /// Idle session lifetime in minutes (`5–10080`).
    pub session_idle_minutes: i32,
    /// Absolute session lifetime in days (`1–365`).
    pub session_absolute_days: i32,
    /// How many sessions one account may hold at once (`1–100`).
    pub session_concurrent_max: i32,
    /// How long a device stays trusted, in days (`0–365`).
    pub device_trust_days: i32,
    /// Whether the organization demands a second factor from its accounts.
    pub mfa_required: bool,
    /// Who last changed the document.
    pub updated_by: Option<Uuid>,
    /// When it last changed.
    pub updated_at: OffsetDateTime,
}

/// The session-relevant slice of the policy, so session code does not carry the whole document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionPolicy {
    /// Idle lifetime in minutes.
    pub idle_minutes: i32,
    /// Absolute lifetime in days.
    pub absolute_days: i32,
    /// Concurrent-session cap.
    pub concurrent_max: i32,
}

impl Default for SessionPolicy {
    fn default() -> Self {
        Self {
            idle_minutes: DEFAULT_IDLE_MINUTES,
            absolute_days: DEFAULT_ABSOLUTE_DAYS,
            concurrent_max: DEFAULT_CONCURRENT_MAX,
        }
    }
}

impl From<&SecurityPolicy> for SessionPolicy {
    fn from(policy: &SecurityPolicy) -> Self {
        Self {
            idle_minutes: policy.session_idle_minutes,
            absolute_days: policy.session_absolute_days,
            concurrent_max: policy.session_concurrent_max,
        }
    }
}

/// One field's change, for the audit entry the save writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyChange {
    /// Column name.
    pub field: &'static str,
    /// Value before the save, rendered for the trail.
    pub before: String,
    /// Value after the save.
    pub after: String,
}

/// The fields a save may change; `None` leaves a field alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolicyPatch {
    /// See [`SecurityPolicy::password_min_length`].
    pub password_min_length: Option<i32>,
    /// See [`SecurityPolicy::password_require_classes`].
    pub password_require_classes: Option<i32>,
    /// See [`SecurityPolicy::password_history`].
    pub password_history: Option<i32>,
    /// See [`SecurityPolicy::password_expiry_days`].
    pub password_expiry_days: Option<i32>,
    /// See [`SecurityPolicy::lockout_attempts`].
    pub lockout_attempts: Option<i32>,
    /// See [`SecurityPolicy::lockout_minutes`].
    pub lockout_minutes: Option<i32>,
    /// See [`SecurityPolicy::ip_allowlist`].
    pub ip_allowlist: Option<Vec<String>>,
    /// See [`SecurityPolicy::ip_denylist`].
    pub ip_denylist: Option<Vec<String>>,
    /// See [`SecurityPolicy::session_idle_minutes`].
    pub session_idle_minutes: Option<i32>,
    /// See [`SecurityPolicy::session_absolute_days`].
    pub session_absolute_days: Option<i32>,
    /// See [`SecurityPolicy::session_concurrent_max`].
    pub session_concurrent_max: Option<i32>,
    /// See [`SecurityPolicy::device_trust_days`].
    pub device_trust_days: Option<i32>,
    /// See [`SecurityPolicy::mfa_required`].
    pub mfa_required: Option<bool>,
}

/// Every column the policy is read with (`cidr[]` is read as text, so no extension type is
/// needed to decode it).
const POLICY_COLUMNS: &str = "organization_id, password_min_length, password_require_classes, \
     password_history, password_expiry_days, lockout_attempts, lockout_minutes, \
     ip_allowlist::text[] as ip_allowlist, ip_denylist::text[] as ip_denylist, \
     session_idle_minutes, session_absolute_days, session_concurrent_max, device_trust_days, \
     mfa_required, updated_by, updated_at";

/// Read the policy of an organization, or `None` when the row does not exist yet.
pub async fn get_policy(
    pool: &PgPool,
    organization_id: Uuid,
) -> Result<Option<SecurityPolicy>> {
    let policy: Option<SecurityPolicy> = sqlx::query_as(&format!(
        "select {POLICY_COLUMNS} from security_policies where organization_id = $1"
    ))
    .bind(organization_id)
    .fetch_optional(pool)
    .await?;
    Ok(policy)
}

/// Read the policy of an organization, creating the conservative default row when missing.
///
/// An installation seeded before a row existed (or an organization created between two
/// migrations) must still answer — the defaults are the table's own, so a created row is
/// indistinguishable from a seeded one.
pub async fn ensure_policy(pool: &PgPool, organization_id: Uuid) -> Result<SecurityPolicy> {
    if let Some(policy) = get_policy(pool, organization_id).await? {
        return Ok(policy);
    }

    sqlx::query("insert into security_policies (organization_id) values ($1) on conflict do nothing")
        .bind(organization_id)
        .execute(pool)
        .await?;

    get_policy(pool, organization_id)
        .await?
        .ok_or(IdentityError::OrganizationNotFound)
}

/// The policy a sign-in reads: the account's organization document, or the defaults.
pub async fn policy_for_account(
    pool: &PgPool,
    organization_id: Option<Uuid>,
) -> Result<Option<SecurityPolicy>> {
    match organization_id {
        Some(organization_id) => ensure_policy(pool, organization_id).await.map(Some),
        None => Ok(None),
    }
}

/// Apply a patch and answer both the state before and after, so the caller can audit the diff.
pub async fn update_policy(
    pool: &PgPool,
    organization_id: Uuid,
    patch: &PolicyPatch,
    updated_by: Option<Uuid>,
) -> Result<(SecurityPolicy, SecurityPolicy)> {
    validate_patch(patch)?;
    let before = ensure_policy(pool, organization_id).await?;

    let after: SecurityPolicy = sqlx::query_as(&format!(
        "update security_policies set \
            password_min_length = coalesce($2, password_min_length), \
            password_require_classes = coalesce($3, password_require_classes), \
            password_history = coalesce($4, password_history), \
            password_expiry_days = coalesce($5, password_expiry_days), \
            lockout_attempts = coalesce($6, lockout_attempts), \
            lockout_minutes = coalesce($7, lockout_minutes), \
            ip_allowlist = coalesce(cast($8 as text[])::cidr[], ip_allowlist), \
            ip_denylist = coalesce(cast($9 as text[])::cidr[], ip_denylist), \
            session_idle_minutes = coalesce($10, session_idle_minutes), \
            session_absolute_days = coalesce($11, session_absolute_days), \
            session_concurrent_max = coalesce($12, session_concurrent_max), \
            device_trust_days = coalesce($13, device_trust_days), \
            mfa_required = coalesce($14, mfa_required), \
            updated_by = $15, \
            updated_at = now() \
         where organization_id = $1 \
         returning {POLICY_COLUMNS}"
    ))
    .bind(organization_id)
    .bind(patch.password_min_length)
    .bind(patch.password_require_classes)
    .bind(patch.password_history)
    .bind(patch.password_expiry_days)
    .bind(patch.lockout_attempts)
    .bind(patch.lockout_minutes)
    .bind(patch.ip_allowlist.as_deref())
    .bind(patch.ip_denylist.as_deref())
    .bind(patch.session_idle_minutes)
    .bind(patch.session_absolute_days)
    .bind(patch.session_concurrent_max)
    .bind(patch.device_trust_days)
    .bind(patch.mfa_required)
    .bind(updated_by)
    .fetch_one(pool)
    .await?;

    Ok((before, after))
}

/// Which fields changed between two states of the document.
#[must_use]
pub fn diff(before: &SecurityPolicy, after: &SecurityPolicy) -> Vec<PolicyChange> {
    let mut changes = Vec::new();
    let mut push = |field: &'static str, before: String, after: String| {
        if before != after {
            changes.push(PolicyChange {
                field,
                before,
                after,
            });
        }
    };

    push(
        "password_min_length",
        before.password_min_length.to_string(),
        after.password_min_length.to_string(),
    );
    push(
        "password_require_classes",
        before.password_require_classes.to_string(),
        after.password_require_classes.to_string(),
    );
    push(
        "password_history",
        before.password_history.to_string(),
        after.password_history.to_string(),
    );
    push(
        "password_expiry_days",
        before.password_expiry_days.to_string(),
        after.password_expiry_days.to_string(),
    );
    push(
        "lockout_attempts",
        before.lockout_attempts.to_string(),
        after.lockout_attempts.to_string(),
    );
    push(
        "lockout_minutes",
        before.lockout_minutes.to_string(),
        after.lockout_minutes.to_string(),
    );
    push(
        "ip_allowlist",
        before.ip_allowlist.join(", "),
        after.ip_allowlist.join(", "),
    );
    push(
        "ip_denylist",
        before.ip_denylist.join(", "),
        after.ip_denylist.join(", "),
    );
    push(
        "session_idle_minutes",
        before.session_idle_minutes.to_string(),
        after.session_idle_minutes.to_string(),
    );
    push(
        "session_absolute_days",
        before.session_absolute_days.to_string(),
        after.session_absolute_days.to_string(),
    );
    push(
        "session_concurrent_max",
        before.session_concurrent_max.to_string(),
        after.session_concurrent_max.to_string(),
    );
    push(
        "device_trust_days",
        before.device_trust_days.to_string(),
        after.device_trust_days.to_string(),
    );
    push(
        "mfa_required",
        before.mfa_required.to_string(),
        after.mfa_required.to_string(),
    );

    changes
}

/// Check every field of a patch against the range the table enforces.
pub fn validate_patch(patch: &PolicyPatch) -> Result<()> {
    range(patch.password_min_length, "password_min_length", 8, 128)?;
    range(
        patch.password_require_classes,
        "password_require_classes",
        1,
        4,
    )?;
    range(patch.password_history, "password_history", 0, 24)?;
    range(
        patch.password_expiry_days,
        "password_expiry_days",
        0,
        730,
    )?;
    range(patch.lockout_attempts, "lockout_attempts", 3, 50)?;
    range(patch.lockout_minutes, "lockout_minutes", 1, 1440)?;
    range(
        patch.session_idle_minutes,
        "session_idle_minutes",
        5,
        10_080,
    )?;
    range(
        patch.session_absolute_days,
        "session_absolute_days",
        1,
        365,
    )?;
    range(
        patch.session_concurrent_max,
        "session_concurrent_max",
        1,
        100,
    )?;
    range(patch.device_trust_days, "device_trust_days", 0, 365)?;

    for (field, list) in [
        ("ip_allowlist", patch.ip_allowlist.as_ref()),
        ("ip_denylist", patch.ip_denylist.as_ref()),
    ] {
        let Some(list) = list else { continue };
        for entry in list {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            Cidr::parse(entry).map_err(|error| match error {
                IdentityError::InvalidNetwork(message) => IdentityError::InvalidPolicy {
                    field: field.to_owned(),
                    message: format!("{entry}: {message}"),
                },
                other => other,
            })?;
        }
    }

    Ok(())
}

/// Range check one optional integer field.
fn range(value: Option<i32>, field: &'static str, min: i32, max: i32) -> Result<()> {
    let Some(value) = value else { return Ok(()) };
    if value < min || value > max {
        return Err(IdentityError::InvalidPolicy {
            field: field.to_owned(),
            message: format!("must be between {min} and {max}"),
        });
    }
    Ok(())
}

/// A network an IP list entry names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    /// Network address.
    pub address: IpAddr,
    /// Prefix length in bits.
    pub prefix: u8,
}

impl Cidr {
    /// Parse `10.0.0.0/8`, `2001:db8::/32` or a bare address (which becomes a `/32` or `/128`).
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        let (address, prefix) = match text.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (text, None),
        };

        let address: IpAddr = address
            .trim()
            .parse()
            .map_err(|_| IdentityError::InvalidNetwork(format!("{address:?} is not an address")))?;

        let max = if address.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(prefix) => prefix.trim().parse::<u8>().map_err(|_| {
                IdentityError::InvalidNetwork(format!("prefix {prefix:?} is not a number"))
            })?,
            None => max,
        };
        if prefix > max {
            return Err(IdentityError::InvalidNetwork(format!(
                "prefix /{prefix} is larger than /{max}"
            )));
        }

        Ok(Self { address, prefix })
    }

    /// Whether `ip` falls inside this network.
    #[must_use]
    pub fn contains(&self, ip: &IpAddr) -> bool {
        match (self.address, ip) {
            (IpAddr::V4(network), IpAddr::V4(candidate)) => {
                let mask = mask_u32(self.prefix);
                (u32::from(network) & mask) == (u32::from(*candidate) & mask)
            }
            (IpAddr::V6(network), IpAddr::V6(candidate)) => {
                let mask = mask_u128(self.prefix);
                (u128::from(network) & mask) == (u128::from(*candidate) & mask)
            }
            _ => false,
        }
    }
}

/// The IPv4 network mask of a prefix length.
fn mask_u32(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    }
}

/// The IPv6 network mask of a prefix length.
fn mask_u128(prefix: u8) -> u128 {
    if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - u32::from(prefix))
    }
}

/// What the IP lists say about an address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpVerdict {
    /// The address may proceed to the next sign-in step.
    Allowed,
    /// The address is refused; `rule` names the entry (or the list) that refused it.
    Denied {
        /// The matching network, or `denylist`/`allowlist` when the whole list is the reason.
        rule: String,
        /// `denylist`, `allowlist` or `undeterminable`.
        reason: &'static str,
    },
}

impl IpVerdict {
    /// `true` when this verdict is a refusal.
    #[must_use]
    pub fn is_denied(&self) -> bool {
        matches!(self, Self::Denied { .. })
    }
}

/// Evaluate the lists: deny wins, an empty allowlist means "any address".
///
/// A request without a known address (in-process tests, a stripped layer) cannot be evaluated;
/// it is allowed, because the address is bookkeeping and never an authorization input on its own
/// — the explicit lists only ever *narrow* who may sign in.
#[must_use]
pub fn check_ip(policy: &SecurityPolicy, ip: Option<IpAddr>) -> IpVerdict {
    let Some(ip) = ip else {
        return IpVerdict::Allowed;
    };

    for entry in &policy.ip_denylist {
        if let Ok(network) = Cidr::parse(entry) {
            if network.contains(&ip) {
                return IpVerdict::Denied {
                    rule: entry.trim().to_owned(),
                    reason: "denylist",
                };
            }
        }
    }

    let allowlist: Vec<&String> = policy
        .ip_allowlist
        .iter()
        .filter(|entry| !entry.trim().is_empty())
        .collect();
    if allowlist.is_empty() {
        return IpVerdict::Allowed;
    }
    for entry in allowlist {
        if let Ok(network) = Cidr::parse(entry) {
            if network.contains(&ip) {
                return IpVerdict::Allowed;
            }
        }
    }

    IpVerdict::Denied {
        rule: policy.ip_allowlist.join(", "),
        reason: "allowlist",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> SecurityPolicy {
        SecurityPolicy {
            organization_id: Uuid::nil(),
            password_min_length: 10,
            password_require_classes: 3,
            password_history: 5,
            password_expiry_days: 0,
            lockout_attempts: 10,
            lockout_minutes: 15,
            ip_allowlist: vec![],
            ip_denylist: vec![],
            session_idle_minutes: 120,
            session_absolute_days: 30,
            session_concurrent_max: 10,
            device_trust_days: 30,
            mfa_required: false,
            updated_by: None,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn ranges_match_the_table_constraints() {
        assert!(validate_patch(&PolicyPatch::default()).is_ok());
        for (field, patch) in [
            (
                "password_min_length",
                PolicyPatch {
                    password_min_length: Some(7),
                    ..PolicyPatch::default()
                },
            ),
            (
                "lockout_attempts",
                PolicyPatch {
                    lockout_attempts: Some(2),
                    ..PolicyPatch::default()
                },
            ),
            (
                "session_idle_minutes",
                PolicyPatch {
                    session_idle_minutes: Some(1),
                    ..PolicyPatch::default()
                },
            ),
            (
                "session_absolute_days",
                PolicyPatch {
                    session_absolute_days: Some(0),
                    ..PolicyPatch::default()
                },
            ),
        ] {
            let error = validate_patch(&patch).expect_err("must be refused");
            match error {
                IdentityError::InvalidPolicy { field: named, .. } => assert_eq!(named, field),
                other => panic!("unexpected error: {other}"),
            }
        }
    }

    #[test]
    fn cidr_parsing_accepts_the_shapes_a_reader_types() {
        let v4 = Cidr::parse("10.1.2.0/24").expect("parses");
        assert!(v4.contains(&"10.1.2.7".parse().expect("ip")));
        assert!(!v4.contains(&"10.1.3.7".parse().expect("ip")));

        let bare = Cidr::parse("192.168.1.5").expect("parses");
        assert!(bare.contains(&"192.168.1.5".parse().expect("ip")));
        assert!(!bare.contains(&"192.168.1.6".parse().expect("ip")));

        let v6 = Cidr::parse("2001:db8::/32").expect("parses");
        assert!(v6.contains(&"2001:db8::1".parse().expect("ip")));
        assert!(!v6.contains(&"2001:db9::1".parse().expect("ip")));
        // Families never match.
        assert!(!v6.contains(&"10.0.0.1".parse().expect("ip")));

        for bad in ["", "not-an-ip", "10.0.0.0/33", "2001:db8::/129", "10.0.0.0/x"] {
            assert!(Cidr::parse(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn deny_wins_and_an_empty_allowlist_allows_everything() {
        let mut document = policy();
        assert_eq!(check_ip(&document, None), IpVerdict::Allowed);
        assert_eq!(
            check_ip(&document, Some("203.0.113.9".parse().expect("ip"))),
            IpVerdict::Allowed
        );

        document.ip_denylist = vec!["203.0.113.0/24".to_owned()];
        let denied = check_ip(&document, Some("203.0.113.9".parse().expect("ip")));
        assert!(denied.is_denied());
        assert_eq!(
            denied,
            IpVerdict::Denied {
                rule: "203.0.113.0/24".to_owned(),
                reason: "denylist"
            }
        );

        // An allowlist narrows, and the denylist still wins over it.
        document.ip_allowlist = vec!["203.0.113.0/24".to_owned()];
        assert!(check_ip(&document, Some("203.0.113.9".parse().expect("ip"))).is_denied());
        assert!(
            check_ip(&document, Some("198.51.100.4".parse().expect("ip"))).is_denied(),
            "outside the allowlist"
        );

        document.ip_denylist.clear();
        assert_eq!(
            check_ip(&document, Some("203.0.113.9".parse().expect("ip"))),
            IpVerdict::Allowed
        );
        assert!(check_ip(&document, Some("198.51.100.4".parse().expect("ip"))).is_denied());
    }

    #[test]
    fn the_diff_names_exactly_what_changed() {
        let before = policy();
        let mut after = before.clone();
        after.lockout_attempts = 5;
        after.mfa_required = true;

        let changes = diff(&before, &after);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].field, "lockout_attempts");
        assert_eq!(changes[0].before, "10");
        assert_eq!(changes[0].after, "5");
        assert_eq!(changes[1].field, "mfa_required");
        assert!(diff(&before, &before).is_empty());
    }

    #[test]
    fn the_session_slice_follows_the_document() {
        let document = policy();
        let slice: SessionPolicy = (&document).into();
        assert_eq!(slice.idle_minutes, 120);
        assert_eq!(slice.absolute_days, 30);
        assert_eq!(slice.concurrent_max, 10);
    }
}
