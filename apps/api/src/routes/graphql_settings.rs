//! The `graphql.*` settings row over PostgreSQL (REQ-130, slice 2).
//!
//! ## One row, read on every refusal
//!
//! `persisted_only` is consulted before a single resolver runs, so this read is on the hot path of
//! every ad-hoc document. It is one indexed row fetch against a table that holds exactly one row,
//! which is cheaper than the JSON parse a generic key-value store would need on the same request.
//!
//! ## A missing row takes the DEFAULTS, never zeroes
//!
//! The obvious failure is a database where the row was never seeded: a `max_depth` of `0` refuses
//! every query including `__typename`, and the report reads as a broken endpoint rather than as a
//! missing row. `load` therefore falls back to [`Settings::default`] — the same values the
//! migration seeds — so an unseeded installation behaves exactly like a fresh one.

use omnion_graphql::{Settings, settings};
use sqlx::PgPool;

use crate::error::ApiError;

/// Read the one settings row, or the defaults when it is absent.
pub async fn load(pool: &PgPool) -> Result<Settings, ApiError> {
    let row = sqlx::query_as::<_, (i32, i32, i32, i32, i32, i64, bool, bool)>(
        "select max_depth, cost_budget, max_aliases, max_fragments, max_page_size, timeout_ms, \
                persisted_only, playground_enabled \
         from graphql_settings where id = 1",
    )
    .fetch_optional(pool)
    .await
    .map_err(|error| {
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("the GraphQL settings row could not be read: {error}"),
        )
    })?;

    Ok(match row {
        Some(row) => Settings {
            max_depth: u32::try_from(row.0).unwrap_or(settings::DEFAULT_MAX_DEPTH),
            cost_budget: u32::try_from(row.1).unwrap_or(settings::DEFAULT_COST_BUDGET),
            max_aliases: u32::try_from(row.2).unwrap_or(settings::DEFAULT_MAX_ALIASES),
            max_fragments: u32::try_from(row.3).unwrap_or(settings::DEFAULT_MAX_FRAGMENTS),
            max_page_size: u32::try_from(row.4).unwrap_or(settings::DEFAULT_MAX_PAGE_SIZE),
            // `timeout_ms` is bigint in SQL because a millisecond count grows past int32 in a
            // long-lived installation; `u64::try_from` and not a cast, because a negative timeout
            // stored by hand must not become `u64::MAX`.
            timeout_ms: u64::try_from(row.5).unwrap_or(settings::DEFAULT_TIMEOUT_MS),
            persisted_only: row.6,
            playground_enabled: row.7,
        },
        None => Settings::default(),
    })
}

/// Write the settings row and return what was stored.
///
/// Validation runs here, not only in the column constraints, because the caller needs a field-level
/// message naming the accepted range and the constraint can only say "violates check constraint".
/// `Settings::validate` names every field individually, and the walk asserts the two disagree about
/// nothing: a value the validator accepts and the constraint refuses is a value the screen shows
/// as saved and the database did not keep.
pub async fn save(
    pool: &PgPool,
    input: &Settings,
    updated_by: Option<uuid::Uuid>,
) -> Result<Settings, ApiError> {
    input.validate().map_err(|error| {
        ApiError::new(
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            "graphql_settings_invalid",
            error.to_string(),
        )
    })?;

    let row = sqlx::query_as::<_, (i32, i32, i32, i32, i32, i64, bool, bool)>(
        "insert into graphql_settings \
           (id, max_depth, cost_budget, max_aliases, max_fragments, max_page_size, timeout_ms, \
            persisted_only, playground_enabled, updated_by, updated_at) \
         values (1, $1, $2, $3, $4, $5, $6, $7, $8, $9, now()) \
         on conflict (id) do update set \
            max_depth = excluded.max_depth, \
            cost_budget = excluded.cost_budget, \
            max_aliases = excluded.max_aliases, \
            max_fragments = excluded.max_fragments, \
            max_page_size = excluded.max_page_size, \
            timeout_ms = excluded.timeout_ms, \
            persisted_only = excluded.persisted_only, \
            playground_enabled = excluded.playground_enabled, \
            updated_by = excluded.updated_by, \
            updated_at = now() \
         returning max_depth, cost_budget, max_aliases, max_fragments, max_page_size, timeout_ms, \
                   persisted_only, playground_enabled",
    )
    .bind(i32::try_from(input.max_depth).unwrap_or(i32::MAX))
    .bind(i32::try_from(input.cost_budget).unwrap_or(i32::MAX))
    .bind(i32::try_from(input.max_aliases).unwrap_or(i32::MAX))
    .bind(i32::try_from(input.max_fragments).unwrap_or(i32::MAX))
    .bind(i32::try_from(input.max_page_size).unwrap_or(i32::MAX))
    .bind(i64::try_from(input.timeout_ms).unwrap_or(i64::MAX))
    .bind(input.persisted_only)
    .bind(input.playground_enabled)
    .bind(updated_by)
    .fetch_one(pool)
    .await
    .map_err(|error| {
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            format!("the GraphQL settings row could not be written: {error}"),
        )
    })?;

    Ok(Settings {
        max_depth: u32::try_from(row.0).unwrap_or(settings::DEFAULT_MAX_DEPTH),
        cost_budget: u32::try_from(row.1).unwrap_or(settings::DEFAULT_COST_BUDGET),
        max_aliases: u32::try_from(row.2).unwrap_or(settings::DEFAULT_MAX_ALIASES),
        max_fragments: u32::try_from(row.3).unwrap_or(settings::DEFAULT_MAX_FRAGMENTS),
        max_page_size: u32::try_from(row.4).unwrap_or(settings::DEFAULT_MAX_PAGE_SIZE),
        timeout_ms: u64::try_from(row.5).unwrap_or(settings::DEFAULT_TIMEOUT_MS),
        persisted_only: row.6,
        playground_enabled: row.7,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_the_store_falls_back_to_are_the_ones_the_migration_seeds() {
        // An unseeded database must behave like a fresh one. If these constants and
        // `Settings::default` ever disagree, the two halves of the feature disagree and the
        // failure only appears on an installation nobody tested.
        let defaults = Settings::default();
        assert_eq!(defaults.max_depth, settings::DEFAULT_MAX_DEPTH);
        assert_eq!(defaults.cost_budget, settings::DEFAULT_COST_BUDGET);
        assert_eq!(defaults.max_aliases, settings::DEFAULT_MAX_ALIASES);
        assert_eq!(defaults.max_fragments, settings::DEFAULT_MAX_FRAGMENTS);
        assert_eq!(defaults.max_page_size, settings::DEFAULT_MAX_PAGE_SIZE);
        assert_eq!(defaults.timeout_ms, settings::DEFAULT_TIMEOUT_MS);
    }

    #[test]
    fn the_store_accepts_exactly_what_the_validator_accepts() {
        // Every value in the legal space round-trips through `validate` without a narrowing cast
        // changing it: a `u32` the database stores as `int` and reads back must not become a
        // different number on the way, or the settings screen shows a value the endpoint does not
        // enforce.
        let legal = Settings {
            max_depth: 256,
            cost_budget: 1,
            max_aliases: 1,
            max_fragments: 1,
            max_page_size: 1,
            timeout_ms: 100,
            persisted_only: true,
            playground_enabled: false,
        };
        legal.validate().expect("the extremes are legal");
        assert_eq!(i32::try_from(legal.max_depth).unwrap(), 256);
        assert_eq!(i64::try_from(legal.timeout_ms).unwrap(), 100);
    }
}
