//! Organizations: the top-level tenant boundary.
//!
//! One organization owns its sites, roles, accounts and settings (docs/07-IAM.md §7,
//! docs/01-VISION.md §10). The row was introduced with the initial schema; this module is the
//! store the API and later phases use instead of talking to the table directly.
//!
//! Slugs follow the same shape the schema enforces (`^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$`) and
//! are unique platform-wide, because a slug is how an organization is addressed before a site
//! domain exists.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{IdentityError, Result};

/// Longest accepted organization name.
const MAX_NAME_LENGTH: usize = 120;

/// Longest accepted slug (matches the schema check).
const MAX_SLUG_LENGTH: usize = 63;

/// Statuses an organization row may carry (matches the schema check).
pub const STATUSES: [&str; 3] = ["active", "suspended", "archived"];

/// The status a freshly created organization starts in.
pub const DEFAULT_STATUS: &str = "active";

/// Column list for every `Organization` query, so the row shape stays in one place.
const ORGANIZATION_COLUMNS: &str = "id, name, slug, status, created_at, updated_at";

/// An organization as stored in the database.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Organization {
    /// Primary key.
    pub id: Uuid,
    /// Display name.
    pub name: String,
    /// Stable handle, unique platform-wide.
    pub slug: String,
    /// `active`, `suspended` or `archived`.
    pub status: String,
    /// Creation timestamp.
    pub created_at: OffsetDateTime,
    /// Last change.
    pub updated_at: OffsetDateTime,
}

impl Organization {
    /// `true` when the organization may operate (only `active` rows serve traffic).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.status == "active"
    }
}

/// An organization to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOrganization {
    /// Display name.
    pub name: String,
    /// Desired slug (normalized before the insert).
    pub slug: String,
}

/// Fields [`update_organization`] may change. `None` leaves a field untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrganizationChanges {
    /// New display name.
    pub name: Option<String>,
    /// New status.
    pub status: Option<String>,
}

impl OrganizationChanges {
    /// `true` when the request changes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.name.is_none() && self.status.is_none()
    }
}

/// Validate an organization name: non-empty and reasonably short.
pub fn validate_name(name: &str) -> Result<String> {
    let name = name.trim().to_owned();
    if name.is_empty() || name.chars().count() > MAX_NAME_LENGTH {
        return Err(IdentityError::InvalidOrganization(format!(
            "name must be 1 to {MAX_NAME_LENGTH} characters"
        )));
    }
    Ok(name)
}

/// Validate an organization slug and normalize it to lowercase.
///
/// Same shape as the schema constraint: lowercase letters, digits and dashes, starting and
/// ending alphanumeric.
pub fn validate_slug(slug: &str) -> Result<String> {
    let slug = slug.trim().to_lowercase();
    let shaped = !slug.is_empty()
        && slug.len() <= MAX_SLUG_LENGTH
        && slug.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && slug.ends_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if shaped {
        return Ok(slug);
    }
    Err(IdentityError::InvalidOrganization(format!(
        "slug {slug:?} must be lowercase letters, digits and dashes (2-63 characters)"
    )))
}

/// Validate an organization status against the schema values.
pub fn validate_status(status: &str) -> Result<String> {
    let status = status.trim().to_lowercase();
    if STATUSES.contains(&status.as_str()) {
        return Ok(status);
    }
    Err(IdentityError::InvalidOrganization(format!(
        "status {status:?} must be one of {}",
        STATUSES.join(", ")
    )))
}

/// Create an organization. Fails with [`IdentityError::OrganizationSlugTaken`] on a taken slug.
pub async fn create_organization(pool: &PgPool, new: NewOrganization) -> Result<Organization> {
    let name = validate_name(&new.name)?;
    let slug = validate_slug(&new.slug)?;

    let sql = format!(
        "insert into organizations (name, slug, status) values ($1, $2, $3) \
         returning {ORGANIZATION_COLUMNS}"
    );

    sqlx::query_as::<_, Organization>(&sql)
        .bind(&name)
        .bind(&slug)
        .bind(DEFAULT_STATUS)
        .fetch_one(pool)
        .await
        .map_err(map_insert_error)
}

/// Look an organization up by id.
pub async fn find_organization(pool: &PgPool, id: Uuid) -> Result<Option<Organization>> {
    let sql = format!("select {ORGANIZATION_COLUMNS} from organizations where id = $1");
    sqlx::query_as::<_, Organization>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

/// Look an organization up by slug (normalized first, so case cannot fork a lookup).
pub async fn find_organization_by_slug(pool: &PgPool, slug: &str) -> Result<Option<Organization>> {
    let slug = validate_slug(slug)?;
    let sql = format!("select {ORGANIZATION_COLUMNS} from organizations where slug = $1");
    sqlx::query_as::<_, Organization>(&sql)
        .bind(&slug)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

/// Every organization, oldest first — the platform operator's view.
pub async fn list_organizations(pool: &PgPool) -> Result<Vec<Organization>> {
    let sql =
        format!("select {ORGANIZATION_COLUMNS} from organizations order by created_at asc, id asc");
    sqlx::query_as::<_, Organization>(&sql)
        .fetch_all(pool)
        .await
        .map_err(Into::into)
}

/// Apply `changes` to an organization. Fails with [`IdentityError::OrganizationNotFound`] when
/// the row is gone; an empty change set returns the current row untouched.
pub async fn update_organization(
    pool: &PgPool,
    id: Uuid,
    changes: &OrganizationChanges,
) -> Result<Organization> {
    let current = find_organization(pool, id)
        .await?
        .ok_or(IdentityError::OrganizationNotFound)?;
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
        "update organizations set \
            name = coalesce($2, name), \
            status = coalesce($3, status), \
            updated_at = now() \
         where id = $1 returning {ORGANIZATION_COLUMNS}"
    );

    sqlx::query_as::<_, Organization>(&sql)
        .bind(id)
        .bind(name)
        .bind(status)
        .fetch_one(pool)
        .await
        .map_err(Into::into)
}

/// Delete an organization; its sites, roles and domains follow through the schema's cascades,
/// while the accounts that belonged to it stay and simply lose their primary organization
/// (`organization_id` is `on delete set null`). `false` when the row was already gone.
///
/// The caller decides whether a populated tenant may go — the store does not look at sites.
pub async fn delete_organization(pool: &PgPool, id: Uuid) -> Result<bool> {
    let deleted = sqlx::query("delete from organizations where id = $1")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected()
        > 0;
    Ok(deleted)
}

fn map_insert_error(err: sqlx::Error) -> IdentityError {
    match err {
        sqlx::Error::Database(ref db_err) if db_err.is_unique_violation() => {
            IdentityError::OrganizationSlugTaken
        }
        other => IdentityError::Database(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_normalized_and_shaped() {
        assert_eq!(
            validate_slug("  Acme-Corp ").expect("valid"),
            "acme-corp",
            "slugs normalize to lowercase"
        );
        for bad in ["", "-lead", "lead-", "lead_corp", "lead corp", "Lead!"] {
            assert!(validate_slug(bad).is_err(), "{bad:?} must be rejected");
        }
        assert!(validate_slug(&"a".repeat(MAX_SLUG_LENGTH + 1)).is_err());
        assert_eq!(validate_slug("a").expect("single letter is fine"), "a");
    }

    #[test]
    fn names_are_trimmed_and_bounded() {
        assert_eq!(
            validate_name("  Acme Corporation ").expect("valid"),
            "Acme Corporation"
        );
        assert!(validate_name("   ").is_err());
        assert!(validate_name(&"n".repeat(MAX_NAME_LENGTH + 1)).is_err());
    }

    #[test]
    fn statuses_come_from_the_schema_set() {
        assert_eq!(validate_status(" Active ").expect("valid"), "active");
        for status in STATUSES {
            assert!(validate_status(status).is_ok());
        }
        assert!(validate_status("deleted").is_err());
        assert!(validate_status("").is_err());
    }

    #[test]
    fn changes_report_emptiness() {
        assert!(OrganizationChanges::default().is_empty());
        assert!(
            !OrganizationChanges {
                name: Some("Acme".to_owned()),
                status: None,
            }
            .is_empty()
        );
    }

    #[test]
    fn not_found_and_taken_errors_are_distinct() {
        assert_eq!(
            IdentityError::OrganizationNotFound.to_string(),
            "no such organization"
        );
        assert_eq!(
            IdentityError::OrganizationSlugTaken.to_string(),
            "organization slug is already taken"
        );
    }
}
