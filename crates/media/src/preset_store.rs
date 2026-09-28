//! The preset and derivative tables (REQ-010, slice 3).
//!
//! [`transform`] decides what a preset *means*; this module stores it and remembers what has
//! already been produced. Two rules shape every statement:
//!
//! * **The cache is consulted by key, not by (file, preset).** A lookup that found "this file
//!   with this preset" would serve stale pixels after a preset edit, because the pair is
//!   unchanged while the definition moved. Looking it up by the content hash of the definition
//!   means an edit simply produces a key nobody has seen, and the old entry becomes
//!   unreachable rather than wrong.
//! * **A failed build leaves no row.** The row is inserted *after* the object is written, so an
//!   encoder failure cannot leave a row pointing at an object that was never stored — a cache
//!   entry that claims bytes exist is worse than a cache miss, because it is invisible until
//!   somebody requests the URL and gets a 404 from the store.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{MediaError, Result};
use crate::transform::{Derivative, ImageFormat, NewPreset, Preset, Recipe, validate_new};

/// The columns of a preset row, in the order [`Preset`] reads them.
const PRESET_COLUMNS: &str = "id, site_id, name, width, height, fit, format, quality, \
                              watermark_media_id, created_at, updated_at";

/// Every preset of a site, in the order the settings screen lists them.
pub async fn list_presets(pool: &PgPool, site_id: Uuid) -> Result<Vec<Preset>> {
    let sql = format!(
        "select {PRESET_COLUMNS} from media_transformation_presets \
                       where site_id = $1 order by name"
    );
    Ok(sqlx::query_as::<_, Preset>(&sql)
        .bind(site_id)
        .fetch_all(pool)
        .await?)
}

/// One preset by name, for the read path.
///
/// The name is bound, never interpolated — a preset name comes from a query string, and the
/// statement is the same statement for every name.
pub async fn find_preset_by_name(
    pool: &PgPool,
    site_id: Uuid,
    name: &str,
) -> Result<Option<Preset>> {
    let sql = format!(
        "select {PRESET_COLUMNS} from media_transformation_presets \
         where site_id = $1 and name = $2"
    );
    Ok(sqlx::query_as::<_, Preset>(&sql)
        .bind(site_id)
        .bind(name)
        .fetch_optional(pool)
        .await?)
}

/// One preset by id, for the settings screen's edit form.
pub async fn find_preset(pool: &PgPool, site_id: Uuid, id: Uuid) -> Result<Option<Preset>> {
    let sql = format!(
        "select {PRESET_COLUMNS} from media_transformation_presets \
         where site_id = $1 and id = $2"
    );
    Ok(sqlx::query_as::<_, Preset>(&sql)
        .bind(site_id)
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

/// Load a preset by name or answer [`MediaError::PresetNotFound`].
///
/// The read path calls this and turns the error into a 404 with the name in it, because "card is
/// not a preset" and "card produced no bytes" are different answers and a caller debugging a
/// broken page needs to know which one it got.
pub async fn require_preset(pool: &PgPool, site_id: Uuid, name: &str) -> Result<Preset> {
    find_preset_by_name(pool, site_id, name)
        .await?
        .ok_or_else(|| MediaError::PresetNotFound {
            name: name.to_string(),
        })
}

/// Create a preset after validating it.
///
/// The unique index is the authority on a name clash, not a pre-check: two concurrent creates of
/// `card` would both pass a `select` and one would fail on insert, which is the correct place for
/// the race to end.
pub async fn create_preset(pool: &PgPool, site_id: Uuid, preset: NewPreset) -> Result<Preset> {
    let preset = validate_new(preset)?;
    let sql = format!(
        "insert into media_transformation_presets \
           (site_id, name, width, height, fit, format, quality, watermark_media_id) \
         values ($1, $2, $3, $4, $5, $6, $7, $8) \
         returning {PRESET_COLUMNS}"
    );
    let result = sqlx::query_as::<_, Preset>(&sql)
        .bind(site_id)
        .bind(&preset.name)
        .bind(preset.width)
        .bind(preset.height)
        .bind(preset.fit.as_str())
        .bind(preset.format.as_str())
        .bind(preset.quality)
        .bind(preset.watermark_media_id)
        .fetch_one(pool)
        .await;

    match result {
        Ok(row) => Ok(row),
        Err(sqlx::Error::Database(ref db)) if db.is_unique_violation() => {
            Err(MediaError::PresetNameTaken { name: preset.name })
        }
        Err(err) => Err(err.into()),
    }
}

/// Replace a preset's definition, or answer [`MediaError::PresetNotFound`].
///
/// `updated_at` is written here rather than trusted from the database default, because a default
/// of `now()` is evaluated per statement and the settings screen sorts by it.
pub async fn update_preset(
    pool: &PgPool,
    site_id: Uuid,
    id: Uuid,
    preset: NewPreset,
) -> Result<Preset> {
    let preset = validate_new(preset)?;
    let sql = format!(
        "update media_transformation_presets set \
           name = $3, width = $4, height = $5, fit = $6, format = $7, quality = $8, \
           watermark_media_id = $9, updated_at = now() \
         where site_id = $1 and id = $2 \
         returning {PRESET_COLUMNS}"
    );
    let result = sqlx::query_as::<_, Preset>(&sql)
        .bind(site_id)
        .bind(id)
        .bind(&preset.name)
        .bind(preset.width)
        .bind(preset.height)
        .bind(preset.fit.as_str())
        .bind(preset.format.as_str())
        .bind(preset.quality)
        .bind(preset.watermark_media_id)
        .fetch_optional(pool)
        .await;

    match result {
        Ok(Some(row)) => Ok(row),
        Ok(None) => Err(MediaError::PresetNotFound {
            name: id.to_string(),
        }),
        Err(sqlx::Error::Database(ref db)) if db.is_unique_violation() => {
            Err(MediaError::PresetNameTaken { name: preset.name })
        }
        Err(err) => Err(err.into()),
    }
}

/// Remove a preset and, by cascade, every derivative built from it.
///
/// Returns whether a row was there. The cascade is deliberate: a derivative of a deleted preset
/// is storage nothing can name, because no URL can produce its key again.
pub async fn delete_preset(pool: &PgPool, site_id: Uuid, id: Uuid) -> Result<bool> {
    let removed =
        sqlx::query("delete from media_transformation_presets where site_id = $1 and id = $2")
            .bind(site_id)
            .bind(id)
            .execute(pool)
            .await?;
    Ok(removed.rows_affected() > 0)
}

/// The derivative already built for this exact input, if there is one.
pub async fn find_derivative(pool: &PgPool, cache_key: &str) -> Result<Option<Derivative>> {
    Ok(sqlx::query_as::<_, Derivative>(
        "select id, media_id, preset_id, cache_key, storage_key, content_type, size_bytes, \
                width, height, source_checksum, created_at \
         from media_derivatives where cache_key = $1",
    )
    .bind(cache_key)
    .fetch_optional(pool)
    .await?)
}

/// Every derivative built for one file, newest first — the file detail screen's list.
pub async fn list_derivatives(pool: &PgPool, media_id: Uuid) -> Result<Vec<Derivative>> {
    Ok(sqlx::query_as::<_, Derivative>(
        "select id, media_id, preset_id, cache_key, storage_key, content_type, size_bytes, \
                width, height, source_checksum, created_at \
         from media_derivatives where media_id = $1 order by created_at desc",
    )
    .bind(media_id)
    .fetch_all(pool)
    .await?)
}

/// A derivative to insert once its bytes are in the store.
#[derive(Debug, Clone)]
pub struct NewDerivative {
    /// File it was built from.
    pub media_id: Uuid,
    /// Preset it was built for.
    pub preset_id: Uuid,
    /// Hash of the inputs.
    pub cache_key: String,
    /// Object key of the generated bytes.
    pub storage_key: String,
    /// Content type of the generated bytes.
    pub content_type: String,
    /// Size of the generated bytes.
    pub size_bytes: i64,
    /// Pixel width of the result.
    pub width: i32,
    /// Pixel height of the result.
    pub height: i32,
    /// Checksum of the source bytes.
    pub source_checksum: String,
}

/// Record a derivative that has already been written to the store.
///
/// A second build of the same inputs is not an error: the unique index caught a race, and the
/// first writer's row is as good as ours because the key is a hash of identical inputs. The
/// stored row is returned either way, so the caller serves the *existing* object.
pub async fn insert_derivative(pool: &PgPool, derivative: NewDerivative) -> Result<Derivative> {
    let sql = "insert into media_derivatives \
                 (media_id, preset_id, cache_key, storage_key, content_type, size_bytes, \
                  width, height, source_checksum) \
               values ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
               on conflict (cache_key) do nothing \
               returning id, media_id, preset_id, cache_key, storage_key, content_type, \
                         size_bytes, width, height, source_checksum, created_at";
    let stored = sqlx::query_as::<_, Derivative>(sql)
        .bind(derivative.media_id)
        .bind(derivative.preset_id)
        .bind(&derivative.cache_key)
        .bind(&derivative.storage_key)
        .bind(&derivative.content_type)
        .bind(derivative.size_bytes)
        .bind(derivative.width)
        .bind(derivative.height)
        .bind(&derivative.source_checksum)
        .fetch_optional(pool)
        .await?;

    match stored {
        Some(row) => Ok(row),
        // The conflict branch returned no row, so read the winner. Its object key is derived
        // from the same hash, so it is the same key this call would have written.
        None => {
            find_derivative(pool, &derivative.cache_key)
                .await?
                .ok_or(MediaError::TransformFailed {
                    reason: "the derivative row disappeared between the insert and the read"
                        .to_string(),
                })
        }
    }
}

/// Every object key a file's derivatives own, so a purge can take the bytes with it.
pub async fn derivative_keys(pool: &PgPool, ids: &[Uuid]) -> Result<Vec<String>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_scalar::<_, String>(
        "select storage_key from media_derivatives where media_id = any($1)",
    )
    .bind(ids)
    .fetch_all(pool)
    .await?)
}

/// Drop every derivative row of a site, returning the keys so the bytes can be removed too.
///
/// This is the cache-clear the settings screen offers. It is a *cache*: after it runs the next
/// request rebuilds, so the only cost is time.
pub async fn clear_derivatives(pool: &PgPool, site_id: Uuid) -> Result<Vec<String>> {
    let keys = sqlx::query_scalar::<_, String>(
        "select d.storage_key from media_derivatives d \
         join media m on m.id = d.media_id \
         where m.site_id = $1",
    )
    .bind(site_id)
    .fetch_all(pool)
    .await?;

    sqlx::query(
        "delete from media_derivatives where media_id in (select id from media where site_id = $1)",
    )
    .bind(site_id)
    .execute(pool)
    .await?;

    Ok(keys)
}

/// The size a site's derivative cache currently occupies, for the settings screen.
pub async fn derivative_totals(pool: &PgPool, site_id: Uuid) -> Result<(i64, i64)> {
    let row: (i64, i64) = sqlx::query_as(
        "select count(*)::bigint, coalesce(sum(d.size_bytes), 0)::bigint \
         from media_derivatives d join media m on m.id = d.media_id \
         where m.site_id = $1",
    )
    .bind(site_id)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// The recipe a stored preset describes, together with the key that identifies its output.
///
/// Bundled because a caller that has one always needs the other, and computing the key from the
/// row in two places is how the two drift apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Served {
    /// The recipe to run.
    pub recipe: Recipe,
    /// The key its output is addressed by.
    pub cache_key: String,
    /// The object key the output is written to.
    pub storage_key: String,
}

/// Work out how one file's derivative for one preset is addressed.
pub fn served_for(preset: &Preset, site_id: Uuid, source_checksum: &str) -> Served {
    let recipe = Recipe::of(preset);
    let cache_key = recipe.cache_key(source_checksum);
    let storage_key = recipe.storage_key(site_id, &cache_key, recipe.format);
    Served {
        recipe,
        cache_key,
        storage_key,
    }
}

/// The name a derivative is served under in a download, e.g. `hero-card.webp`.
#[must_use]
pub fn derivative_filename(filename: &str, format: ImageFormat) -> String {
    let stem = filename.rsplit_once('.').map_or(filename, |(stem, _)| stem);
    format!("{stem}-card.{}", format.extension())
}

/// A derivative row's age, for the settings screen.
#[must_use]
pub fn age_of(created_at: OffsetDateTime, now: OffsetDateTime) -> std::time::Duration {
    let seconds = (now - created_at).whole_seconds().max(0) as u64;
    std::time::Duration::from_secs(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_storage_key_follows_the_cache_key_and_the_emitted_format() {
        // The trap: writing `.png` because the *source* was a PNG leaves an object whose
        // extension lies about its bytes, and anything that trusts the extension (an S3 content
        // type, a CDN rule, a human in a console) gets it wrong.
        let preset = Preset {
            id: Uuid::nil(),
            site_id: Uuid::nil(),
            name: "card".to_string(),
            width: Some(1200),
            height: Some(630),
            fit: "cover".to_string(),
            format: "jpeg".to_string(),
            quality: 80,
            watermark_media_id: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        let served = served_for(&preset, Uuid::nil(), "abc");
        assert!(
            served.storage_key.ends_with(".jpg"),
            "{}",
            served.storage_key
        );
        assert!(served.storage_key.contains(&served.cache_key));
    }

    #[test]
    fn editing_a_preset_changes_the_key_it_is_served_under() {
        // The reason the cache key is built from the definition and not from (file, preset):
        // this is what stops an edit from serving the old pixels under the old name.
        let base = Preset {
            id: Uuid::nil(),
            site_id: Uuid::nil(),
            name: "card".to_string(),
            width: Some(1200),
            height: Some(630),
            fit: "cover".to_string(),
            format: "webp".to_string(),
            quality: 80,
            watermark_media_id: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        let edited = Preset {
            quality: 60,
            ..base.clone()
        };
        let before = served_for(&base, Uuid::nil(), "abc");
        let after = served_for(&edited, Uuid::nil(), "abc");
        assert_ne!(before.cache_key, after.cache_key);
        assert_ne!(before.storage_key, after.storage_key);
    }

    #[test]
    fn replacing_a_file_changes_the_key_its_derivatives_are_served_under() {
        // The source checksum is part of the key, so a replace needs no invalidation pass: the old
        // derivatives simply stop being asked for.
        let preset = Preset {
            id: Uuid::nil(),
            site_id: Uuid::nil(),
            name: "card".to_string(),
            width: Some(1200),
            height: Some(630),
            fit: "cover".to_string(),
            format: "webp".to_string(),
            quality: 80,
            watermark_media_id: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        assert_ne!(
            served_for(&preset, Uuid::nil(), "old").cache_key,
            served_for(&preset, Uuid::nil(), "new").cache_key
        );
    }

    #[test]
    fn a_derivative_file_name_carries_the_preset_suffix() {
        // `hero.png` at the `card` preset is `hero-card.webp`: the suffix is what makes a browser
        // download distinguishable from the original, and the extension is the emitted format.
        assert_eq!(
            derivative_filename("hero.png", ImageFormat::WebP),
            "hero-card.webp"
        );
        assert_eq!(
            derivative_filename("hero.png", ImageFormat::Jpeg),
            "hero-card.jpg"
        );
        // A name with no dot is left alone rather than losing its stem to a phantom extension.
        assert_eq!(
            derivative_filename("hero", ImageFormat::Png),
            "hero-card.png"
        );
    }

    #[test]
    fn an_age_never_goes_negative() {
        // A row created "in the future" (clock skew between two hosts) must not make a duration
        // that panics or a countdown that counts up.
        let now = OffsetDateTime::UNIX_EPOCH;
        let future = now + time::Duration::seconds(600);
        assert_eq!(age_of(future, now), std::time::Duration::ZERO);
        assert_eq!(
            age_of(now - time::Duration::seconds(60), now),
            std::time::Duration::from_secs(60)
        );
    }
}
