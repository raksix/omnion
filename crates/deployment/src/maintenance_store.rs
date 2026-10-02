//! The maintenance window's storage, behind the `store` feature.
//!
//! Split out of [`crate::maintenance`] for the same reason [`crate::jobs`] carries a gate: the
//! decisions in that module are worth testing with no database, and while the four queries below
//! sat in the same file the crate's own default test command could not compile at all. Every test
//! in `maintenance` is a decision test — an unscheduled window that blocks, a scope that blocks
//! the panel and not the public write, a message that is counted in characters — and none of them
//! needed this file to move; they needed it to stop sharing a module with SQL.
//!
//! The window is a single row per environment, so "the unset shape" is a real answer rather than
//! an error: an environment that has never had a window configured blocks nothing, and the panel
//! says so instead of the API answering `404`.

#![cfg(feature = "store")]

use time::OffsetDateTime;

use crate::error::StoreError;
use crate::maintenance::{Block, Scope, Window};

/// Load one environment's window, or the unset shape.
pub async fn load_window(pool: &sqlx::PgPool, environment: &str) -> Result<Window, StoreError> {
    let row: Option<(
        bool,
        String,
        Option<OffsetDateTime>,
        Option<OffsetDateTime>,
        String,
        Option<uuid::Uuid>,
        OffsetDateTime,
    )> = sqlx::query_as(
        "select enabled, message, starts_at, ends_at, scope, updated_by, updated_at \
             from maintenance_windows where environment = $1",
    )
    .bind(environment)
    .fetch_optional(pool)
    .await?;

    let Some((enabled, message, starts_at, ends_at, scope, updated_by, updated_at)) = row else {
        return Ok(Window::unset(environment));
    };
    // An unreadable scope makes the row refuse *nothing* rather than guess: the same refusal
    // the wrong way as `Scope::parse`, in the place where guessing would block a platform.
    let scope = Scope::parse(&scope).unwrap_or(Scope::All);
    Ok(Window {
        environment: environment.to_string(),
        enabled,
        message,
        starts_at,
        ends_at,
        scope,
        updated_by,
        updated_at: Some(updated_at),
    })
}

/// Every configured window, for the shell banner and the screen's overview.
pub async fn list_windows(pool: &sqlx::PgPool) -> Result<Vec<Window>, StoreError> {
    let rows: Vec<(
        String,
        bool,
        String,
        Option<OffsetDateTime>,
        Option<OffsetDateTime>,
        String,
        Option<uuid::Uuid>,
        OffsetDateTime,
    )> = sqlx::query_as(
        "select environment, enabled, message, starts_at, ends_at, scope, updated_by, updated_at \
             from maintenance_windows order by environment",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(environment, enabled, message, starts_at, ends_at, scope, updated_by, updated_at)| {
                Window {
                    environment,
                    enabled,
                    message,
                    starts_at,
                    ends_at,
                    scope: Scope::parse(&scope).unwrap_or(Scope::All),
                    updated_by,
                    updated_at: Some(updated_at),
                }
            },
        )
        .collect())
}

/// Store a window. The caller has already checked the refusals via [`save`].
pub async fn store_window(
    pool: &sqlx::PgPool,
    window: &Window,
    actor: Option<uuid::Uuid>,
) -> Result<(), StoreError> {
    sqlx::query(
        "insert into maintenance_windows (environment, enabled, message, starts_at, ends_at, scope, updated_by, updated_at) \
         values ($1, $2, $3, $4, $5, $6, $7, now()) \
         on conflict (environment) do update set enabled = excluded.enabled, message = excluded.message, \
           starts_at = excluded.starts_at, ends_at = excluded.ends_at, scope = excluded.scope, \
           updated_by = excluded.updated_by, updated_at = now()",
    )
    .bind(&window.environment)
    .bind(window.enabled)
    .bind(&window.message)
    .bind(window.starts_at)
    .bind(window.ends_at)
    .bind(window.scope.as_str())
    .bind(actor)
    .execute(pool)
    .await?;
    Ok(())
}

/// The window that currently blocks a write, across every environment.
///
/// Returns the **first active window for this environment**; `None` when writes are allowed.
/// One query rather than three, because the write routes call it on every request and a
/// deployment centre that slows down the rest of the platform to enforce its own banner is a
/// worse outage than the one it prevents.
pub async fn active_blocker(
    pool: &sqlx::PgPool,
    environment: &str,
) -> Result<Option<Block>, StoreError> {
    let row: Option<(String,)> = sqlx::query_as(
        "select message from maintenance_windows \
         where environment = $1 and enabled = true \
           and (starts_at is null or starts_at <= now()) \
           and (ends_at is null or ends_at > now())",
    )
    .bind(environment)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(message,)| Block { message }))
}
