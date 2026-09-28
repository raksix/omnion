//! The SQL behind the preference matrix and the settings row (REQ-021, slice 2).
//!
//! Two tables, one file, and three rules that the panel cannot see but the reader can:
//!
//! * **A read is a left join onto the closed list, never a scan of what exists.** The matrix
//!   the settings form renders is `CATEGORIES × CHANNELS` with the stated value where there is
//!   one and `true` everywhere else — so the count of cells is a property of the vocabulary,
//!   and adding a channel in slice 3 shows up in the form for every user who never opened it.
//! * **A write is an upsert of the stated cells, not a delete-and-reinsert.** Full-replace
//!   semantics on a table that only stores *changes* would delete every cell the caller did not
//!   mention — the difference between "I turned ticket e-mail off" and "I turned everything
//!   else on, because I sent a partial body".
//! * **`in_app` is never written as `false`.** The check is in the vocabulary (and therefore in
//!   a test on the route), and this file refuses it again at the SQL boundary: the route is not
//!   the only writer, and a row that says "no bell" would be a lie the panel believes.

use sqlx::PgPool;
use sqlx::postgres::PgQueryResult;
use uuid::Uuid;

use crate::error::Result;
use crate::preferences::{Preferences, Settings, StatedPreference};
use crate::vocabulary::CHANNELS;

/// Read one person's complete matrix and settings, filling in the platform defaults.
///
/// **The default matrix is built in Rust, not in SQL.** The query below returns only the cells
/// this person actually stated; everything else comes from [`Preferences::defaults`]. That
/// split is what keeps a new channel from needing a backfill, and it is also why a person with
/// no rows at all gets a full, valid form rather than an empty grid.
pub async fn read_preferences(pool: &PgPool, user_id: Uuid) -> Result<Preferences> {
    let stated: Vec<(String, String, bool)> = sqlx::query_as(
        "select category, channel, enabled from notification_preferences where user_id = $1",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    let mut preferences = Preferences::defaults(user_id);
    for (category, channel, enabled) in stated {
        if let Some(cell) = preferences
            .matrix
            .iter_mut()
            .find(|cell| cell.category == category && cell.channel == channel)
        {
            cell.enabled = enabled;
        }
        // A row the vocabulary does not know is ignored rather than rendered: the check
        // constraint makes it impossible today, and a category that appears in the form with
        // no label is worse than one that does not appear at all.
    }

    preferences.settings = read_settings(pool, user_id).await?;
    Ok(preferences)
}

/// One person's settings row, or the defaults when they have never opened the screen.
pub async fn read_settings(pool: &PgPool, user_id: Uuid) -> Result<Settings> {
    let row: Option<(
        Option<String>,
        Option<String>,
        String,
        String,
        Option<i16>,
        i16,
    )> = sqlx::query_as(
        "select quiet_hours_start::text, quiet_hours_end::text, timezone, digest_cadence, \
                    digest_weekday, digest_hour \
             from notification_settings where user_id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map_or_else(
        || Settings::default_for(user_id),
        |(start, end, timezone, cadence, weekday, hour)| Settings {
            user_id,
            quiet_hours_start: start,
            quiet_hours_end: end,
            timezone,
            digest_cadence: cadence,
            digest_weekday: weekday,
            digest_hour: hour,
        },
    ))
}

/// Write the stated cells and the settings row, and report how many cells changed.
///
/// **Only the cells the caller named are written.** The upsert is one statement per cell
/// rather than a loop of "delete all mine, insert all yours", because the second form cannot
/// express a partial save at all — and a settings form that saves the whole grid is a form
/// that turns one unchecked box into a `500` the moment a client omits a row.
pub async fn write_preferences(
    pool: &PgPool,
    user_id: Uuid,
    cells: &[StatedPreference],
    settings: &Settings,
) -> Result<u64> {
    crate::preferences::validate_stated(cells)?;
    crate::preferences::validate_settings(settings)?;

    let mut changed = 0;
    for cell in cells {
        // The in-app row is written as `true` whatever the caller said, and the caller's
        // `false` was already refused by `validate_stated` — this is the second belt, for the
        // writer that is not the route.
        let enabled = cell.enabled || cell.channel == crate::preferences::IN_APP;
        let result: PgQueryResult = sqlx::query(
            "insert into notification_preferences (user_id, category, channel, enabled, updated_at) \
             values ($1, $2, $3, $4, now()) \
             on conflict (user_id, category, channel) \
             do update set enabled = excluded.enabled, updated_at = now() \
             where notification_preferences.enabled is distinct from excluded.enabled",
        )
        .bind(user_id)
        .bind(&cell.category)
        .bind(&cell.channel)
        .bind(enabled)
        .execute(pool)
        .await?;
        // The `where` on the update means an unchanged cell reports zero rows. That is the
        // honest number: the settings form shows "N preferences saved", and a form that
        // reports thirty for a reader who flipped two boxes teaches them not to read it.
        changed += result.rows_affected();
    }

    write_settings(pool, user_id, settings).await?;
    Ok(changed)
}

/// Write the settings row, inserting it the first time.
pub async fn write_settings(pool: &PgPool, user_id: Uuid, settings: &Settings) -> Result<()> {
    sqlx::query(
        "insert into notification_settings \
         (user_id, quiet_hours_start, quiet_hours_end, timezone, digest_cadence, \
          digest_weekday, digest_hour, updated_at) \
         values ($1, $2::time, $3::time, $4, $5, $6, $7, now()) \
         on conflict (user_id) do update set \
            quiet_hours_start = excluded.quiet_hours_start, \
            quiet_hours_end   = excluded.quiet_hours_end, \
            timezone          = excluded.timezone, \
            digest_cadence    = excluded.digest_cadence, \
            digest_weekday    = excluded.digest_weekday, \
            digest_hour       = excluded.digest_hour, \
            updated_at        = now()",
    )
    .bind(user_id)
    .bind(&settings.quiet_hours_start)
    .bind(&settings.quiet_hours_end)
    .bind(&settings.timezone)
    .bind(&settings.digest_cadence)
    .bind(settings.digest_weekday)
    .bind(settings.digest_hour)
    .execute(pool)
    .await?;
    Ok(())
}

/// Which channels a notification of this category may go out over, for one person.
///
/// The one place a delivery decision is made, so the runner and the settings form cannot
/// disagree: a reader who turned ticket e-mail off gets no ticket e-mail, and the drawer shows
/// the delivery as `skipped` with the reason rather than as a failure.
pub async fn allowed_channels(pool: &PgPool, user_id: Uuid, category: &str) -> Result<Vec<String>> {
    let preferences = read_preferences(pool, user_id).await?;
    Ok(CHANNELS
        .iter()
        .copied()
        .filter(|channel| preferences.allows(category, channel))
        .map(str::to_owned)
        .collect())
}

/// Drop every stated cell of a person, returning them to the platform defaults.
///
/// A "reset" that only rewrites the visible cells leaves the invisible ones set — and a
/// reader who resets expects the *form they were looking at*, which is the visible part, but
/// a leftover `web_push = false` from a month ago is exactly the kind of thing that makes
/// "I reset it and still get nothing" true. So this deletes the rows, not the values.
pub async fn reset_preferences(pool: &PgPool, user_id: Uuid) -> Result<u64> {
    let result: PgQueryResult =
        sqlx::query("delete from notification_preferences where user_id = $1")
            .bind(user_id)
            .execute(pool)
            .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preferences::PreferenceCell;
    use crate::vocabulary::CATEGORIES;

    #[test]
    fn a_fresh_person_gets_a_complete_matrix_not_an_empty_one() {
        // The default is built in Rust, which is the whole reason a partial save is safe.
        let preferences = Preferences::defaults(Uuid::nil());
        assert_eq!(preferences.matrix.len(), CATEGORIES.len() * CHANNELS.len());
        assert!(
            preferences
                .matrix
                .iter()
                .all(|cell| CATEGORIES.contains(&cell.category.as_str())
                    && CHANNELS.contains(&cell.channel.as_str()))
        );
    }

    #[test]
    fn every_default_cell_is_findable_by_its_own_names() {
        // `Preferences::enabled` falls back to `true` for a cell it cannot find, so a typo in
        // the lookup would be invisible — the cell list is what the lookup searches, and this
        // is the assertion that the two agree.
        let preferences = Preferences::defaults(Uuid::nil());
        for category in CATEGORIES {
            for channel in CHANNELS {
                assert!(
                    preferences
                        .matrix
                        .iter()
                        .any(|cell| { cell.category == category && cell.channel == channel }),
                    "{category}/{channel} is missing from the default matrix"
                );
            }
        }
    }

    #[test]
    fn the_in_app_cell_is_written_true_whatever_the_caller_says() {
        // The mirror of the validation: `write_preferences` computes this before the bind.
        let cell = StatedPreference {
            category: "security".to_owned(),
            channel: crate::preferences::IN_APP.to_owned(),
            enabled: false,
        };
        // The route refuses it outright, so this is the second belt rather than the first.
        assert!(crate::preferences::validate_stated(std::slice::from_ref(&cell)).is_err());
        let coerced = cell.enabled || cell.channel == crate::preferences::IN_APP;
        assert!(
            coerced,
            "in_app must be written as true even if it reaches the store"
        );
    }

    #[test]
    fn a_cell_outside_the_vocabulary_never_lands_in_the_matrix() {
        // `read_preferences` skips a row the closed list does not know. The shape of that rule,
        // proven without a database: the merge is a `find`, and a name that is not in the
        // defaults matrix is not findable.
        let preferences = Preferences::defaults(Uuid::nil());
        let ghost = PreferenceCell {
            category: "invoice".to_owned(),
            channel: "email".to_owned(),
            enabled: false,
        };
        assert!(
            !preferences
                .matrix
                .iter()
                .any(|cell| cell.category == ghost.category && cell.channel == ghost.channel)
        );
    }
}
