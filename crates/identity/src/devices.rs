//! Known devices: the registry behind "new device" notices and the trust window.
//!
//! A device is a fingerprint — a hash of the normalized user agent, never a cookie a client
//! could forge into someone else's device — plus what the panel shows about it (label,
//! platform, browser) and how long it stays trusted (docs/07-IAM.md §15). Sign-in upserts the
//! row, so first/last seen are real observations rather than declared values, and `forget`
//! revokes instead of deleting: a forgotten device that comes back is visible again as a new
//! one, and the history of the row survives.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use sha2::{Digest, Sha256};

use crate::error::Result;

/// Longest user agent kept when describing a device.
const MAX_AGENT_LENGTH: usize = 400;

/// A registered device.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Device {
    /// Primary key.
    pub id: Uuid,
    /// Owning account.
    pub user_id: Uuid,
    /// Hash of the normalized user agent.
    pub fingerprint: String,
    /// Human label (`Chrome on Linux`).
    pub label: String,
    /// Operating system family.
    pub platform: String,
    /// Browser family.
    pub browser: String,
    /// When the device first signed in.
    pub first_seen_at: OffsetDateTime,
    /// When it last signed in.
    pub last_seen_at: OffsetDateTime,
    /// Until when the device counts as trusted.
    pub trusted_until: Option<OffsetDateTime>,
    /// When it was forgotten.
    pub revoked_at: Option<OffsetDateTime>,
    /// When the row was created.
    pub created_at: OffsetDateTime,
}

/// A device with the account it belongs to, for the panel's table.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct DeviceView {
    /// Device id.
    pub id: Uuid,
    /// Owning account.
    pub user_id: Uuid,
    /// Account address.
    pub user_email: String,
    /// Account display name.
    pub user_display_name: String,
    /// Human label.
    pub label: String,
    /// Operating system family.
    pub platform: String,
    /// Browser family.
    pub browser: String,
    /// Short, non-reversible fingerprint prefix (safe to show).
    pub fingerprint_hint: String,
    /// First observation.
    pub first_seen_at: OffsetDateTime,
    /// Last observation.
    pub last_seen_at: OffsetDateTime,
    /// Trust window end.
    pub trusted_until: Option<OffsetDateTime>,
    /// Whether the device was forgotten.
    pub revoked: bool,
    /// Live sessions recorded on this device.
    pub session_count: i64,
}

/// Column list of a device row.
const DEVICE_COLUMNS: &str = "id, user_id, fingerprint, label, platform, browser, first_seen_at, \
     last_seen_at, trusted_until, revoked_at, created_at";

/// Hash a user agent into a stable fingerprint.
///
/// The same browser on the same platform hashes identically (the agent string is trimmed and
/// whitespace-collapsed first); a client that sends nothing still gets a fingerprint — of the
/// empty agent — so the registry stays honest about "we cannot tell these apart".
#[must_use]
pub fn fingerprint(user_agent: Option<&str>) -> String {
    let normalized = normalize_agent(user_agent);
    let mut hasher = Sha256::new();
    hasher.update(b"omnion.device.v1");
    hasher.update(normalized.as_bytes());
    hex::encode(hasher.finalize())
}

/// Trim and collapse the whitespace of a user agent.
fn normalize_agent(user_agent: Option<&str>) -> String {
    let raw = user_agent.unwrap_or_default();
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(MAX_AGENT_LENGTH).collect()
}

/// Describe a user agent as (label, platform, browser) with conservative heuristics.
#[must_use]
pub fn describe(user_agent: Option<&str>) -> (String, String, String) {
    let agent = normalize_agent(user_agent);
    let platform = if agent.contains("Android") {
        "Android"
    } else if agent.contains("iPhone") || agent.contains("iPad") {
        "iOS"
    } else if agent.contains("Windows") {
        "Windows"
    } else if agent.contains("Macintosh") || agent.contains("Mac OS X") {
        "macOS"
    } else if agent.contains("Linux") {
        "Linux"
    } else {
        "Unknown"
    };

    let browser = if agent.contains("Edg/") {
        "Edge"
    } else if agent.contains("OPR/") || agent.contains("Opera") {
        "Opera"
    } else if agent.contains("Firefox/") {
        "Firefox"
    } else if agent.contains("Chrome/") && !agent.contains("Chromium/") {
        "Chrome"
    } else if agent.contains("Chromium/") {
        "Chromium"
    } else if agent.contains("Safari/") {
        "Safari"
    } else if agent.contains("curl/") {
        "curl"
    } else {
        "Unknown"
    };

    let label = if platform == "Unknown" && browser == "Unknown" {
        "Unidentified device".to_owned()
    } else if browser == "Unknown" {
        format!("{platform} device")
    } else if platform == "Unknown" {
        format!("{browser} client")
    } else {
        format!("{browser} on {platform}")
    };

    (label, platform.to_owned(), browser.to_owned())
}

/// Register (or refresh) the device a sign-in came from, and return its row.
pub async fn upsert(
    pool: &PgPool,
    user_id: Uuid,
    user_agent: Option<&str>,
    trust_days: i32,
) -> Result<Device> {
    let fingerprint = fingerprint(user_agent);
    let (label, platform, browser) = describe(user_agent);
    let trusted_until = (trust_days > 0)
        .then(|| OffsetDateTime::now_utc() + time::Duration::days(i64::from(trust_days)));

    let device: Device = sqlx::query_as(&format!(
        "insert into user_devices \
            (user_id, fingerprint, label, platform, browser, trusted_until) \
         values ($1, $2, $3, $4, $5, $6) \
         on conflict (user_id, fingerprint) do update set \
            label = excluded.label, \
            platform = excluded.platform, \
            browser = excluded.browser, \
            last_seen_at = now(), \
            trusted_until = coalesce(user_devices.trusted_until, excluded.trusted_until), \
            revoked_at = null \
         returning {DEVICE_COLUMNS}"
    ))
    .bind(user_id)
    .bind(&fingerprint)
    .bind(&label)
    .bind(&platform)
    .bind(&browser)
    .bind(trusted_until)
    .fetch_one(pool)
    .await?;

    Ok(device)
}

/// Filters of the device list.
#[derive(Debug, Clone, Default)]
pub struct DeviceFilter {
    /// Only devices of this account.
    pub user_id: Option<Uuid>,
    /// Only devices of accounts in this organization.
    pub organization_id: Option<Uuid>,
    /// Free text over label, platform, browser and account address.
    pub search: Option<String>,
    /// Include forgotten devices.
    pub include_revoked: bool,
}

/// List devices for the panel, newest observation first.
pub async fn list(pool: &PgPool, filter: &DeviceFilter) -> Result<Vec<DeviceView>> {
    let search = filter
        .search
        .as_ref()
        .map(|value| format!("%{}%", value.trim().to_lowercase()));

    let rows: Vec<DeviceView> = sqlx::query_as(
        "select d.id, d.user_id, u.email as user_email, u.display_name as user_display_name, \
                d.label, d.platform, d.browser, left(d.fingerprint, 12) as fingerprint_hint, \
                d.first_seen_at, d.last_seen_at, d.trusted_until, \
                (d.revoked_at is not null) as revoked, \
                (select count(*) from sessions s \
                  where s.device_id = d.id and s.revoked_at is null and s.expires_at > now()) \
                    as session_count \
         from user_devices d \
         join users u on u.id = d.user_id \
         where ($1::uuid is null or d.user_id = $1) \
           and ($2::uuid is null or u.organization_id = $2) \
           and ($3::bool or d.revoked_at is null) \
           and ($4::text is null or d.label ilike $4 or d.platform ilike $4 \
                or d.browser ilike $4 or u.email ilike $4) \
         order by d.last_seen_at desc \
         limit 500",
    )
    .bind(filter.user_id)
    .bind(filter.organization_id)
    .bind(filter.include_revoked)
    .bind(search.as_deref())
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Read one device.
pub async fn find(pool: &PgPool, id: Uuid) -> Result<Option<Device>> {
    let device: Option<Device> = sqlx::query_as(&format!(
        "select {DEVICE_COLUMNS} from user_devices where id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(device)
}

/// Set (or clear) the trust window of a device.
pub async fn set_trust(
    pool: &PgPool,
    id: Uuid,
    trusted_until: Option<OffsetDateTime>,
) -> Result<Option<Device>> {
    let device: Option<Device> = sqlx::query_as(&format!(
        "update user_devices set trusted_until = $2 where id = $1 returning {DEVICE_COLUMNS}"
    ))
    .bind(id)
    .bind(trusted_until)
    .fetch_optional(pool)
    .await?;
    Ok(device)
}

/// Forget a device: the row is revoked, never deleted.
pub async fn forget(pool: &PgPool, id: Uuid) -> Result<Option<Device>> {
    let device: Option<Device> = sqlx::query_as(&format!(
        "update user_devices set revoked_at = now() where id = $1 returning {DEVICE_COLUMNS}"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(device)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_agent_fingerprints_the_same_way() {
        let chrome = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
                      Chrome/151.0 Safari/537.36";
        assert_eq!(fingerprint(Some(chrome)), fingerprint(Some(chrome)));
        // Whitespace differences do not fork a device into two rows.
        let spaced = chrome.split_whitespace().collect::<Vec<_>>().join("   ");
        assert_eq!(fingerprint(Some(chrome)), fingerprint(Some(&spaced)));
        assert_ne!(fingerprint(Some(chrome)), fingerprint(None));
        assert_eq!(fingerprint(None).len(), 64);
    }

    #[test]
    fn descriptions_name_the_family_a_reader_recognises() {
        let (label, platform, browser) = describe(Some(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
             Chrome/151.0.0.0 Safari/537.36 Edg/151.0",
        ));
        assert_eq!(platform, "Windows");
        assert_eq!(browser, "Edge");
        assert_eq!(label, "Edge on Windows");

        let (_, platform, browser) =
            describe(Some("Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) Safari/604.1"));
        assert_eq!(platform, "iOS");
        assert_eq!(browser, "Safari");

        let (label, platform, browser) = describe(None);
        assert_eq!(platform, "Unknown");
        assert_eq!(browser, "Unknown");
        assert_eq!(label, "Unidentified device");

        let (label, _, browser) = describe(Some("curl/8.5.0"));
        assert_eq!(browser, "curl");
        assert_eq!(label, "curl client");
    }
}
