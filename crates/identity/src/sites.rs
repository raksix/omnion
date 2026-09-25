//! Sites: the properties an organization publishes.
//!
//! An organization owns one or more sites (docs/01-VISION.md §10 "Multi-site vs multi-tenant");
//! each site carries the domains that address it. Hosts are unique platform-wide and every site
//! keeps at most one primary domain, which later phases (themes, canonical links, sitemaps)
//! build their absolute URLs from.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{IdentityError, Result};

/// Longest accepted site name.
const MAX_NAME_LENGTH: usize = 120;

/// Longest accepted site key (matches the schema check).
const MAX_KEY_LENGTH: usize = 63;

/// Longest accepted host (DNS name limit).
const MAX_HOST_LENGTH: usize = 253;

/// Longest accepted single label of a host.
const MAX_LABEL_LENGTH: usize = 63;

/// Statuses a site row may carry (matches the schema check).
pub const SITE_STATUSES: [&str; 2] = ["active", "archived"];

/// The status a freshly created site starts in.
pub const DEFAULT_STATUS: &str = "active";

/// Column list for every `Site` query.
const SITE_COLUMNS: &str = "id, organization_id, key, name, status, created_at, updated_at";

/// Column list for every `SiteDomain` query.
const DOMAIN_COLUMNS: &str = "id, site_id, host, is_primary, created_at, updated_at";

/// A site as stored in the database.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Site {
    /// Primary key.
    pub id: Uuid,
    /// Owning organization.
    pub organization_id: Uuid,
    /// Stable handle inside the organization.
    pub key: String,
    /// Display name.
    pub name: String,
    /// `active` or `archived`.
    pub status: String,
    /// Creation timestamp.
    pub created_at: OffsetDateTime,
    /// Last change.
    pub updated_at: OffsetDateTime,
}

impl Site {
    /// `true` when the site serves traffic.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.status == "active"
    }
}

/// A domain that addresses a site.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct SiteDomain {
    /// Primary key.
    pub id: Uuid,
    /// The site it addresses.
    pub site_id: Uuid,
    /// Host name, lowercase.
    pub host: String,
    /// Whether the site prefers this domain for absolute links.
    pub is_primary: bool,
    /// Creation timestamp.
    pub created_at: OffsetDateTime,
    /// Last change.
    pub updated_at: OffsetDateTime,
}

/// A site to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSite {
    /// Owning organization.
    pub organization_id: Uuid,
    /// Desired key (normalized before the insert).
    pub key: String,
    /// Display name.
    pub name: String,
}

/// Fields [`update_site`] may change. `None` leaves a field untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SiteChanges {
    /// New display name.
    pub name: Option<String>,
    /// New status.
    pub status: Option<String>,
}

impl SiteChanges {
    /// `true` when the request changes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.name.is_none() && self.status.is_none()
    }
}

/// Validate a site key and normalize it to lowercase.
///
/// Same shape as the schema constraint: lowercase letters, digits and dashes, starting and
/// ending alphanumeric (`main`, `careers-tr`).
pub fn validate_key(key: &str) -> Result<String> {
    let key = key.trim().to_lowercase();
    let shaped = !key.is_empty()
        && key.len() <= MAX_KEY_LENGTH
        && key.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && key.ends_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if shaped {
        return Ok(key);
    }
    Err(IdentityError::InvalidSite(format!(
        "key {key:?} must be lowercase letters, digits and dashes (1-63 characters)"
    )))
}

/// Validate a site name: non-empty and reasonably short.
pub fn validate_name(name: &str) -> Result<String> {
    let name = name.trim().to_owned();
    if name.is_empty() || name.chars().count() > MAX_NAME_LENGTH {
        return Err(IdentityError::InvalidSite(format!(
            "name must be 1 to {MAX_NAME_LENGTH} characters"
        )));
    }
    Ok(name)
}

/// Validate a site status against the schema values.
pub fn validate_status(status: &str) -> Result<String> {
    let status = status.trim().to_lowercase();
    if SITE_STATUSES.contains(&status.as_str()) {
        return Ok(status);
    }
    Err(IdentityError::InvalidSite(format!(
        "status {status:?} must be one of {}",
        SITE_STATUSES.join(", ")
    )))
}

/// Validate a domain host and normalize it to lowercase.
///
/// Hosts are stored lowercase so one host cannot be registered twice; each dot-separated label
/// must be 1-63 characters of letters, digits and dashes (no leading or trailing dash).
pub fn validate_host(host: &str) -> Result<String> {
    let host = host.trim().to_lowercase();
    let valid = !host.is_empty()
        && host.len() <= MAX_HOST_LENGTH
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= MAX_LABEL_LENGTH
                && label.starts_with(|c: char| c.is_ascii_alphanumeric())
                && label.ends_with(|c: char| c.is_ascii_alphanumeric())
                && label
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        });
    if valid {
        return Ok(host);
    }
    Err(IdentityError::InvalidHost(host))
}

/// Create a site inside an organization.
///
/// Fails with [`IdentityError::SiteKeyTaken`] when the organization already owns the key.
pub async fn create_site(pool: &PgPool, new: NewSite) -> Result<Site> {
    let key = validate_key(&new.key)?;
    let name = validate_name(&new.name)?;

    let sql = format!(
        "insert into sites (organization_id, key, name, status) values ($1, $2, $3, $4) \
         returning {SITE_COLUMNS}"
    );

    sqlx::query_as::<_, Site>(&sql)
        .bind(new.organization_id)
        .bind(&key)
        .bind(&name)
        .bind(DEFAULT_STATUS)
        .fetch_one(pool)
        .await
        .map_err(map_site_insert_error)
}

/// Look a site up by id.
pub async fn find_site(pool: &PgPool, id: Uuid) -> Result<Option<Site>> {
    let sql = format!("select {SITE_COLUMNS} from sites where id = $1");
    sqlx::query_as::<_, Site>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

/// Look a site up by organization and key.
pub async fn find_site_by_key(
    pool: &PgPool,
    organization_id: Uuid,
    key: &str,
) -> Result<Option<Site>> {
    let key = validate_key(key)?;
    let sql = format!("select {SITE_COLUMNS} from sites where organization_id = $1 and key = $2");
    sqlx::query_as::<_, Site>(&sql)
        .bind(organization_id)
        .bind(&key)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

/// Every site of one organization, oldest first.
pub async fn list_sites_for_organization(
    pool: &PgPool,
    organization_id: Uuid,
) -> Result<Vec<Site>> {
    let sql = format!(
        "select {SITE_COLUMNS} from sites where organization_id = $1 \
         order by created_at asc, id asc"
    );
    sqlx::query_as::<_, Site>(&sql)
        .bind(organization_id)
        .fetch_all(pool)
        .await
        .map_err(Into::into)
}

/// Number of sites an organization owns (the emptiness check before a tenant is deleted).
pub async fn count_for_organization(pool: &PgPool, organization_id: Uuid) -> Result<i64> {
    let count: i64 = sqlx::query_scalar("select count(*) from sites where organization_id = $1")
        .bind(organization_id)
        .fetch_one(pool)
        .await?;
    Ok(count)
}

/// Every site of the platform, grouped by organization — the operator's view.
pub async fn list_sites(pool: &PgPool) -> Result<Vec<Site>> {
    let sql = format!(
        "select {SITE_COLUMNS} from sites order by organization_id asc, created_at asc, id asc"
    );
    sqlx::query_as::<_, Site>(&sql)
        .fetch_all(pool)
        .await
        .map_err(Into::into)
}

/// Apply `changes` to a site. Fails with [`IdentityError::SiteNotFound`] when the row is gone;
/// an empty change set returns the current row untouched.
pub async fn update_site(pool: &PgPool, id: Uuid, changes: &SiteChanges) -> Result<Site> {
    let current = find_site(pool, id)
        .await?
        .ok_or(IdentityError::SiteNotFound)?;
    if changes.is_empty() {
        return Ok(current);
    }

    let name = match &changes.name {
        Some(name) => Some(validate_name(name)?),
        None => None,
    };
    let status = match &changes.status {
        Some(status) => Some(validate_status(status)?),
        None => None,
    };

    let sql = format!(
        "update sites set \
            name = coalesce($2, name), \
            status = coalesce($3, status), \
            updated_at = now() \
         where id = $1 returning {SITE_COLUMNS}"
    );

    sqlx::query_as::<_, Site>(&sql)
        .bind(id)
        .bind(name)
        .bind(status)
        .fetch_one(pool)
        .await
        .map_err(Into::into)
}

/// Delete a site; its domains follow through the schema's cascade. `false` when it was gone.
pub async fn delete_site(pool: &PgPool, id: Uuid) -> Result<bool> {
    let deleted = sqlx::query("delete from sites where id = $1")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected()
        > 0;
    Ok(deleted)
}

/// Bind a host to a site.
///
/// The first domain of a site becomes its primary one; `is_primary` promotes the new host when
/// asked. Fails with [`IdentityError::DomainTaken`] when the host already addresses a site.
pub async fn add_domain(
    pool: &PgPool,
    site_id: Uuid,
    host: &str,
    is_primary: bool,
) -> Result<SiteDomain> {
    let host = validate_host(host)?;
    if find_site(pool, site_id).await?.is_none() {
        return Err(IdentityError::SiteNotFound);
    }

    let mut tx = pool.begin().await?;
    let sql = format!(
        "insert into site_domains (site_id, host, is_primary) values ($1, $2, false) \
         returning {DOMAIN_COLUMNS}"
    );
    let domain: SiteDomain = sqlx::query_as(&sql)
        .bind(site_id)
        .bind(&host)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_domain_insert_error)?;

    let has_primary: bool = sqlx::query_scalar(
        "select exists (select 1 from site_domains where site_id = $1 and is_primary)",
    )
    .bind(site_id)
    .fetch_one(&mut *tx)
    .await?;

    if is_primary || !has_primary {
        make_primary(&mut tx, site_id, domain.id).await?;
    }

    let domain: SiteDomain = sqlx::query_as(&format!(
        "select {DOMAIN_COLUMNS} from site_domains where id = $1"
    ))
    .bind(domain.id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(domain)
}

/// Domains of a site, primary first.
pub async fn list_domains(pool: &PgPool, site_id: Uuid) -> Result<Vec<SiteDomain>> {
    let sql = format!(
        "select {DOMAIN_COLUMNS} from site_domains where site_id = $1 \
         order by is_primary desc, created_at asc, id asc"
    );
    sqlx::query_as::<_, SiteDomain>(&sql)
        .bind(site_id)
        .fetch_all(pool)
        .await
        .map_err(Into::into)
}

/// Remove a domain from a site and return the row that was removed.
///
/// When the primary domain goes away, the oldest remaining domain takes its place so an active
/// site with domains always has exactly one primary.
pub async fn remove_domain(
    pool: &PgPool,
    site_id: Uuid,
    domain_id: Uuid,
) -> Result<Option<SiteDomain>> {
    let mut tx = pool.begin().await?;
    let sql = format!(
        "delete from site_domains where id = $1 and site_id = $2 returning {DOMAIN_COLUMNS}"
    );
    let removed: Option<SiteDomain> = sqlx::query_as(&sql)
        .bind(domain_id)
        .bind(site_id)
        .fetch_optional(&mut *tx)
        .await?;

    if removed.as_ref().is_some_and(|domain| domain.is_primary) {
        let next: Option<Uuid> = sqlx::query_scalar(
            "select id from site_domains where site_id = $1 order by created_at asc, id asc limit 1",
        )
        .bind(site_id)
        .fetch_optional(&mut *tx)
        .await?;

        if let Some(next) = next {
            make_primary(&mut tx, site_id, next).await?;
        }
    }

    tx.commit().await?;
    Ok(removed)
}

/// Make one domain of a site the primary one.
pub async fn set_primary_domain(
    pool: &PgPool,
    site_id: Uuid,
    domain_id: Uuid,
) -> Result<SiteDomain> {
    let belongs: bool = sqlx::query_scalar(
        "select exists (select 1 from site_domains where id = $1 and site_id = $2)",
    )
    .bind(domain_id)
    .bind(site_id)
    .fetch_one(pool)
    .await?;
    if !belongs {
        return Err(IdentityError::DomainNotFound);
    }

    let mut tx = pool.begin().await?;
    make_primary(&mut tx, site_id, domain_id).await?;
    let domain: SiteDomain = sqlx::query_as(&format!(
        "select {DOMAIN_COLUMNS} from site_domains where id = $1"
    ))
    .bind(domain_id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(domain)
}

/// Resolve an incoming Host header to its site — the routing primitive the public renderer
/// builds on.
pub async fn find_site_by_host(pool: &PgPool, host: &str) -> Result<Option<Site>> {
    let host = validate_host(host)?;
    // Column list qualified by hand: `id`, `created_at` and `updated_at` exist on both sides
    // of the join, so an unqualified list would be ambiguous.
    let sql = "select s.id, s.organization_id, s.key, s.name, s.status, s.created_at, s.updated_at \
               from sites s join site_domains d on d.site_id = s.id where d.host = $1";
    sqlx::query_as::<_, Site>(sql)
        .bind(&host)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

/// Set `domain_id` as the primary domain of `site_id` and clear the flag from its siblings.
///
/// Two statements on purpose: a single updating statement can trip the partial unique index
/// while it swaps the flag between two rows.
async fn make_primary(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    site_id: Uuid,
    domain_id: Uuid,
) -> Result<()> {
    sqlx::query(
        "update site_domains set is_primary = false, updated_at = now() \
         where site_id = $1 and is_primary and id <> $2",
    )
    .bind(site_id)
    .bind(domain_id)
    .execute(&mut **tx)
    .await?;

    sqlx::query(
        "update site_domains set is_primary = true, updated_at = now() where id = $1 and site_id = $2",
    )
    .bind(domain_id)
    .bind(site_id)
    .execute(&mut **tx)
    .await?;

    Ok(())
}

/// A unique violation on `sites` is the per-organization key; on `site_domains` it is the
/// platform-wide host. The two tables need their own mapping, so a taken key is never reported
/// as a taken host.
fn map_site_insert_error(err: sqlx::Error) -> IdentityError {
    match err {
        sqlx::Error::Database(ref db_err) if db_err.is_unique_violation() => {
            IdentityError::SiteKeyTaken
        }
        other => IdentityError::Database(other),
    }
}

fn map_domain_insert_error(err: sqlx::Error) -> IdentityError {
    match err {
        sqlx::Error::Database(ref db_err) if db_err.is_unique_violation() => {
            IdentityError::DomainTaken
        }
        other => IdentityError::Database(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_keys_are_slugs() {
        assert_eq!(validate_key(" Careers-TR ").expect("valid"), "careers-tr");
        for bad in ["", "-main", "main-", "main_site", "main site", "Main!"] {
            assert!(validate_key(bad).is_err(), "{bad:?} must be rejected");
        }
        assert!(validate_key(&"a".repeat(MAX_KEY_LENGTH + 1)).is_err());
    }

    #[test]
    fn hosts_normalize_and_reject_bad_labels() {
        assert_eq!(
            validate_host(" WWW.Acme.COM ").expect("valid"),
            "www.acme.com"
        );
        assert_eq!(
            validate_host("localhost").expect("single label is fine"),
            "localhost"
        );
        for bad in [
            "",
            "-acme.com",
            "acme-.com",
            "acme..com",
            "acme.com.",
            ".acme.com",
            "acme_corp.com",
            "acme .com",
        ] {
            assert!(validate_host(bad).is_err(), "{bad:?} must be rejected");
        }
        assert!(validate_host(&format!("{}.com", "a".repeat(MAX_LABEL_LENGTH + 1))).is_err());
        assert!(validate_host(&"a.".repeat(MAX_HOST_LENGTH)).is_err());
    }

    #[test]
    fn names_and_statuses_are_bounded() {
        assert_eq!(validate_name("  Main Site ").expect("valid"), "Main Site");
        assert!(validate_name(" ").is_err());

        assert_eq!(validate_status(" Archived ").expect("valid"), "archived");
        assert!(validate_status("paused").is_err());
    }

    #[test]
    fn changes_report_emptiness() {
        assert!(SiteChanges::default().is_empty());
        assert!(
            !SiteChanges {
                name: None,
                status: Some("archived".to_owned()),
            }
            .is_empty()
        );
    }
}
