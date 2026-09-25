//! Translation rows of the content surface (docs/01-VISION.md §5, §6).
//!
//! Translations are rows, not columns: `translations` carries one value of one field of one
//! resource in one language, so a new language never needs a migration — and no `title_tr`
//! style columns exist anywhere. v0 writes them for page revisions; the same table already
//! accepts other resource types, which is what the Translation Center and the Translation
//! Memory build on.

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{ContentError, Result};
use crate::model::{NewRevisionTranslation, REVISION_RESOURCE, Translation};
use crate::validation::{validate_field, validate_language, validate_translation_value};

/// Column list for every `Translation` query.
const TRANSLATION_COLUMNS: &str = "id, organization_id, resource_type, resource_id, language, \
     field, value, created_by, created_at, updated_at";

/// Write (or overwrite) one translated value of a revision field.
///
/// The organization is derived from the revision's page → site chain, so a translation row can
/// never name a different tenant than the resource it belongs to. Writing the same
/// language/field pair again updates the value in place. Fails with
/// [`ContentError::RevisionNotFound`] when the revision does not exist.
pub async fn set_revision_translation(
    pool: &PgPool,
    new: NewRevisionTranslation,
) -> Result<Translation> {
    let language = validate_language(&new.language)?;
    let field = validate_field(&new.field)?;
    let value = validate_translation_value(&new.value)?;

    let organization_id: Option<Uuid> = sqlx::query_scalar(
        "select s.organization_id from page_revisions r \
         join pages p on p.id = r.page_id \
         join sites s on s.id = p.site_id where r.id = $1",
    )
    .bind(new.revision_id)
    .fetch_optional(pool)
    .await?;
    let Some(organization_id) = organization_id else {
        return Err(ContentError::RevisionNotFound);
    };

    let sql = format!(
        "insert into translations \
         (organization_id, resource_type, resource_id, language, field, value, created_by) \
         values ($1, $2, $3, $4, $5, $6, $7) \
         on conflict (resource_type, resource_id, language, field) do update set \
         value = excluded.value, updated_at = now(), created_by = excluded.created_by \
         returning {TRANSLATION_COLUMNS}"
    );
    let stored: Translation = sqlx::query_as(&sql)
        .bind(organization_id)
        .bind(REVISION_RESOURCE)
        .bind(new.revision_id)
        .bind(&language)
        .bind(&field)
        .bind(&value)
        .bind(new.created_by)
        .fetch_one(pool)
        .await?;

    Ok(stored)
}

/// Every translation row of one revision, ordered by language and field.
pub async fn revision_translations(pool: &PgPool, revision_id: Uuid) -> Result<Vec<Translation>> {
    let sql = format!(
        "select {TRANSLATION_COLUMNS} from translations \
         where resource_type = $1 and resource_id = $2 \
         order by language asc, field asc"
    );
    sqlx::query_as::<_, Translation>(&sql)
        .bind(REVISION_RESOURCE)
        .bind(revision_id)
        .fetch_all(pool)
        .await
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_revision_resource_key_matches_the_schema() {
        // The schema constrains `resource_type` to a lowercase key; `page_revision` satisfies
        // it and is the value v0 writes.
        assert!(
            REVISION_RESOURCE
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == '_')
        );
    }
}
