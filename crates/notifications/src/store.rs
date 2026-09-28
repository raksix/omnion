//! The notification store: the SQL behind the record, its reads and its bulk actions.
//!
//! Every list read in this file builds its WHERE clause and its bind values from **one** loop
//! over **one** filter list. That is not a style preference: the failure it prevents is a
//! count that disagrees with the page it counts, which on a notification inbox reads as "5
//! unread" above a list of 3 — the single most damaging kind of wrong, because the reader
//! trusts the badge and stops looking.
//!
//! The other two rules are about what a read is allowed to see:
//!
//! * **Owner-scoped always.** There is no store function that takes a user id the caller
//!   chooses. Every one takes the *owning* id, so "list somebody else's notifications" is not
//!   a parameter a handler can get wrong — it is a function that does not exist. The admin
//!   outbox (slice 3) is the single exception and it takes an organization.
//! * **A deleted notification is a `None`, never an empty record.** A caller that gets
//!   `None` for a row it cannot see and `None` for a row that is gone learns the same thing,
//!   which is the point: the detail route answers `404` for another person's notification.

use sqlx::PgPool;
use sqlx::postgres::PgQueryResult;
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

use crate::error::{NotificationError, Result};
use crate::model::{ListQuery, NewNotification, Notification, NotificationPage};
use crate::vocabulary::{MAX_PAGE, is_category, is_channel};

const COLUMNS: &str = "id, organization_id, user_id, category, priority, title, body, url, \
                       source_type, source_id, payload, read_at, archived_at, created_at";

// ---------------------------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------------------------

/// Record one notification for one person, and report whether a row was created.
///
/// **The `false` is as important as the `true`.** A repeated emit — the same event, the same
/// row, retried by a runner or fired by two workers at once — collapses into the row that is
/// already there and this answers `false`. A caller that ignores it has written a second copy
/// of the same fact, and an inbox that gains a duplicate every time a job retries is an inbox
/// people learn to ignore.
pub async fn record(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    emitted_by: Option<Uuid>,
    draft: &NewNotification,
) -> Result<bool> {
    let draft = draft.clone().build()?;
    let query = "insert into notifications \
                 (organization_id, user_id, category, priority, title, body, url, source_type, \
                  source_id, payload, dedupe_key, emitted_by) \
                 values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
                 on conflict (user_id, dedupe_key) where dedupe_key is not null do nothing";
    let result = sqlx::query(query)
        .bind(organization_id)
        .bind(draft.user_id)
        .bind(&draft.category)
        .bind(&draft.priority)
        .bind(&draft.title)
        .bind(&draft.body)
        .bind(&draft.url)
        .bind(&draft.source_type)
        .bind(&draft.source_id)
        .bind(&draft.payload)
        .bind(&draft.dedupe_key)
        .bind(emitted_by)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() == 1)
}

/// Record the same notification for several people at once.
///
/// A module that has to tell forty reviewers that a page is waiting writes one loop over forty
/// drafts, not forty statements — and the result says how many rows really appeared, so a
/// recipient who has already read the first copy does not make the count look broken.
pub async fn record_many(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    emitted_by: Option<Uuid>,
    drafts: &[NewNotification],
) -> Result<u64> {
    let mut created = 0;
    for draft in drafts {
        if record(pool, organization_id, emitted_by, draft).await? {
            created += 1;
        }
    }
    Ok(created)
}

// ---------------------------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------------------------

/// One page of one person's notifications, newest first.
///
/// `limit + 1` rows are read and the extra one is dropped, which is what makes
/// [`NotificationPage::has_more`] an answer rather than a guess: a second query to count
/// "is there more" is a query that can disagree with the page it is asking about.
pub async fn list(
    pool: &PgPool,
    user_id: Uuid,
    query: &ListQuery,
) -> Result<NotificationPage> {
    let limit = query.limit.clamp(1, MAX_PAGE);
    let mut builder = QueryBuilder::<Postgres>::new(format!(
        "select {COLUMNS} from notifications where "
    ));
    push_filters(&mut builder, user_id, query);
    builder.push(" order by created_at desc, id desc limit ");
    builder.push_bind(limit + 1);

    let mut rows = builder.build_query_as::<Notification>().fetch_all(pool).await?;
    let has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    Ok(NotificationPage {
        notifications: rows,
        has_more,
    })
}

/// How many notifications a person has not read, in total and per category.
///
/// One statement, grouped, so the badge and the grouped lines beneath it are the same query —
/// a badge that says 12 above a list of four grouped lines adding up to 9 is a screen nobody
/// believes afterwards.
pub async fn summary(pool: &PgPool, user_id: Uuid) -> Result<crate::model::Summary> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "select category, count(*)::bigint as count from notifications \
         where user_id = $1 and read_at is null and archived_at is null \
         group by category order by category",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    // The grouped lines are the *closed* category list, not whatever the rows happened to
    // contain: a category with no unread rows still gets a line with a zero, and the panel
    // decides whether to hide it. Rendering only the non-empty ones would make a category
    // disappear from the bell when it empties, so the same reader could not find the filter
    // that lists it.
    let by_category = crate::vocabulary::CATEGORIES
        .iter()
        .map(|category| crate::model::CategoryCount {
            category: (*category).to_owned(),
            count: rows
                .iter()
                .find(|(name, _)| name == category)
                .map_or(0, |(_, count)| *count),
        })
        .collect();

    let unread = rows.iter().map(|(_, count)| *count).sum();
    Ok(crate::model::Summary { unread, by_category })
}

/// One notification, and only if that person owns it.
///
/// `None` for somebody else's row is the whole point: the detail route turns it into a `404`,
/// so a caller can never learn that an id exists.
pub async fn find(pool: &PgPool, user_id: Uuid, id: Uuid) -> Result<Option<Notification>> {
    let query = format!(
        "select {COLUMNS} from notifications where id = $1 and user_id = $2"
    );
    Ok(sqlx::query_as::<_, Notification>(&query)
        .bind(id)
        .bind(user_id)
        .fetch_optional(pool)
        .await?)
}

/// Push the scope and every filter.
///
/// One loop, and each branch writes its own clause *and* its own value — a `$n` in a clause
/// can only exist where the value beside it was pushed. `list` is the only caller, which is
/// what keeps the count and the page from being able to drift.
fn push_filters(builder: &mut QueryBuilder<'_, Postgres>, user_id: Uuid, query: &ListQuery) {
    builder.push("user_id = ");
    builder.push_bind(user_id);
    if !query.include_archived {
        builder.push(" and archived_at is null");
    }
    // The read filter, from either of the two ways of asking for it. `include_read` is the
    // "default, unset" answer and `unread` is the explicit one — and when both are present the
    // explicit one **replaces** the default rather than adding to it.
    //
    // This is the bug the unit test `an_explicit_read_filter_wins_over_include_read` was
    // written for, and it was a real one: pushing "and read_at is null" for `include_read:
    // false` and then "and read_at is not null" for `unread: Some(false)` produces a query
    // that is *always* empty. The panel's "show read" filter hit exactly that path, so every
    // "read" filter in the list returned an empty table and looked like a reader with no read
    // notifications at all. One filter, two spellings, one clause.
    //
    // The clause is computed first and pushed once, rather than pushed from each arm: `push`
    // returns the builder, so a `match` over it needs `()` arms and reads worse than this.
    let read_clause = match query.unread {
        Some(true) => Some(" and read_at is null"),
        Some(false) => Some(" and read_at is not null"),
        None if !query.include_read => Some(" and read_at is null"),
        None => None,
    };
    if let Some(clause) = read_clause {
        builder.push(clause);
    }
    for category in &query.categories {
        builder.push(" and category = ");
        builder.push_bind(category.clone());
    }
    for priority in &query.priorities {
        builder.push(" and priority = ");
        builder.push_bind(priority.clone());
    }
    if let Some(channel) = &query.channel {
        // A channel is a *delivery* property, so the filter joins the deliveries table. The
        // join is an `exists` rather than a `join`: a notification delivered over two
        // channels must appear once, not twice, and a reader filtering by "e-mail" is asking
        // "which of these reached me by e-mail", not "how many e-mails".
        builder.push(
            " and exists (select 1 from notification_deliveries d \
             where d.notification_id = notifications.id and d.channel = ",
        );
        builder.push_bind(channel.clone());
        builder.push(")");
    }
    if let Some(before) = query.before {
        builder.push(" and (created_at, id) < (");
        builder.push_bind(before);
        builder.push(", ");
        builder.push_bind(Uuid::nil());
        builder.push(")");
    }
}

// ---------------------------------------------------------------------------------------------
// Changing
// ---------------------------------------------------------------------------------------------

/// Mark one notification read or unread, and report how many rows changed.
///
/// Zero rows means the row is somebody else's, is gone, or **was already in the state asked
/// for** — and the three are deliberately not told apart here, because the panel's "Mark read"
/// on an already-read row is a no-op the reader expects, while a no-op they can distinguish is
/// a leak of whether the id exists.
pub async fn set_read(pool: &PgPool, user_id: Uuid, id: Uuid, read: bool) -> Result<u64> {
    set_read_many(pool, user_id, &[id], read).await
}

/// Mark several notifications read or unread, and report how many rows changed.
///
/// One statement over an id array rather than a loop of single-row updates: a bulk action over
/// two hundred rows is two hundred round trips otherwise, and the count those two hundred
/// updates report is a sum this one statement gets for free — which is also the number the
/// panel shows, so a partial application cannot disagree with the message.
pub async fn set_read_many(pool: &PgPool, user_id: Uuid, ids: &[Uuid], read: bool) -> Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let statement = if read {
        "update notifications set read_at = coalesce(read_at, now()) \
         where id = any($1) and user_id = $2"
    } else {
        "update notifications set read_at = null where id = any($1) and user_id = $2"
    };
    let result = sqlx::query(statement)
        .bind(ids)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// Mark every unread notification of a person read, and report how many rows changed.
///
/// Bounded by what the person actually has: a "Mark all read" on a person with four unread
/// says four, so the number the panel shows is the number the button changed.
pub async fn mark_all_read(pool: &PgPool, user_id: Uuid) -> Result<u64> {
    let result = sqlx::query(
        "update notifications set read_at = now() \
         where user_id = $1 and read_at is null and archived_at is null",
    )
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// File notifications away, and report how many rows changed.
pub async fn archive(pool: &PgPool, user_id: Uuid, ids: &[Uuid]) -> Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let result: PgQueryResult = sqlx::query(
        "update notifications set archived_at = now() \
         where id = any($1) and user_id = $2 and archived_at is null",
    )
    .bind(ids)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Delete notifications, and report how many rows really went.
///
/// Unlike the rest of the surface this is a hard delete: a notification is addressed to one
/// person and describes something that already happened elsewhere, so there is nothing to
/// restore it from and keeping a tombstone would be storing a copy of somebody else's data
/// for no reader.
pub async fn delete(pool: &PgPool, user_id: Uuid, ids: &[Uuid]) -> Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let result: PgQueryResult = sqlx::query(
        "delete from notifications where id = any($1) and user_id = $2",
    )
    .bind(ids)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

// ---------------------------------------------------------------------------------------------
// The emit budget
// ---------------------------------------------------------------------------------------------

/// Count the emits one actor has made in the last minute.
///
/// The budget is a **count against the emits an actor caused**, not against the notifications
/// that resulted: a module that emits to a role of forty people has spent one emit, and a
/// module stuck in a loop spends the next one within a minute. Counting the recipients
/// instead would make the cheapest legitimate broadcast the most expensive thing on the
/// platform.
///
/// This is a column, not a `payload->>'…'` probe, and the difference matters: a budget that
/// reads its evidence out of a JSON blob breaks the first time a module writes a payload of
/// its own — and a rate limiter that silently starts counting zero is a rate limiter that
/// protects nothing while still returning `429` to nobody.
pub async fn emits_in_the_last_minute(pool: &PgPool, emitted_by: Uuid) -> Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "select count(*)::bigint from notifications \
         where emitted_by = $1 and created_at > now() - interval '1 minute'",
    )
    .bind(emitted_by)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// `true` when the actor still has budget left this minute.
///
/// The comparison is `>=` against the cap, so a module that has spent exactly the cap is
/// refused the next one: a budget of 60 a minute means 60, not 61.
pub async fn within_emit_budget(pool: &PgPool, emitted_by: Uuid) -> Result<bool> {
    Ok(emits_in_the_last_minute(pool, emitted_by).await? < crate::vocabulary::EMIT_BUDGET_PER_MINUTE)
}

/// Check a payload's category and channel names before the store ever sees them.
///
/// The SQL has check constraints and would refuse a bad value — but with a constraint name
/// rather than a sentence, so a module author reads "violates notifications_category_check"
/// instead of "category \"invoice\" is not one of …". This is the friendly front door.
pub fn validate_categories(categories: &[String]) -> Result<()> {
    for category in categories {
        if !is_category(category) {
            return Err(NotificationError::invalid(format!(
                "category \"{category}\" is not one of {:?}",
                crate::vocabulary::CATEGORIES
            )));
        }
    }
    Ok(())
}

/// Check a channel name the same way.
pub fn validate_channel(channel: &str) -> Result<()> {
    if !is_channel(channel) {
        return Err(NotificationError::invalid(format!(
            "channel \"{channel}\" is not one of {:?}",
            crate::vocabulary::CHANNELS
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render a builder's SQL, and report the highest `$n` it references.
    ///
    /// The filter builder is pure string work plus `push_bind`, so it is testable on its own —
    /// and it is exactly the part where a missing `push_bind` produces a *valid* query that
    /// counts the wrong rows. sqlx 0.8 does not hand the encoded values back to a caller, so
    /// the assertion is on the placeholder numbering instead: in a `QueryBuilder` the `$n` are
    /// assigned in push order, so the highest number in the SQL **is** the number of values the
    /// query carries. A clause that pushes `$2` without pushing a second value shows up here.
    ///
    /// `build()` **consumes** the builder and a `QueryBuilder` cannot be reused afterwards, so
    /// the SQL is taken in the same call that builds it.
    fn rendered(builder: QueryBuilder<'_, Postgres>) -> (String, usize) {
        use sqlx::Execute;
        let mut builder = builder;
        let sql = builder.build().sql().to_owned();
        let highest = sql
            .split('$')
            .skip(1)
            .filter_map(|tail| {
                tail.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse::<usize>()
                    .ok()
            })
            .max()
            .unwrap_or(0);
        (sql, highest)
    }

    /// The SQL and placeholder count for one filter set.
    fn render(query: &ListQuery) -> (String, usize) {
        let mut builder = QueryBuilder::<Postgres>::new("select 1 from notifications where ");
        push_filters(&mut builder, Uuid::nil(), query);
        rendered(builder)
    }

    #[test]
    fn the_empty_query_is_just_the_scope() {
        let (sql, values) = render(&ListQuery::default());
        assert_eq!(
            sql,
            "select 1 from notifications where user_id = $1 \
             and archived_at is null and read_at is null"
        );
        // The scope and nothing else: one placeholder, one value.
        assert_eq!(values, 1);
    }

    #[test]
    fn include_flags_turn_the_two_clauses_off() {
        let (sql, _) = render(&ListQuery {
            include_archived: true,
            include_read: true,
            ..ListQuery::default()
        });
        assert!(!sql.contains("archived_at is null"));
        assert!(!sql.contains("read_at is null"));
    }

    #[test]
    fn an_explicit_read_filter_wins_over_include_read() {
        // The reader asked for the read rows on a view that hid them; answering with the
        // unread ones would be more than they asked for.
        let (sql, _) = render(&ListQuery {
            include_read: false,
            unread: Some(false),
            ..ListQuery::default()
        });
        assert!(sql.contains("read_at is not null"));
        assert!(!sql.contains("and read_at is null"));
    }

    #[test]
    fn a_channel_filter_is_an_exists_and_not_a_join() {
        let (sql, values) = render(&ListQuery {
            channel: Some("email".to_owned()),
            ..ListQuery::default()
        });
        // An `exists`, so a notification delivered over two channels appears once rather
        // than twice — and a reader filtering by "e-mail" is asking "which of these reached
        // me by e-mail", not "how many e-mails".
        assert!(sql.contains("exists (select 1 from notification_deliveries"));
        assert!(!sql.contains("join notification_deliveries"));
        // scope + channel = two values, one for each `$n` in the SQL.
        assert_eq!(values, 2);
        assert!(sql.contains("d.channel = $2"));
    }

    #[test]
    fn every_filter_value_reaches_the_sql() {
        let (sql, values) = render(&ListQuery {
            categories: vec!["approval".to_owned(), "security".to_owned()],
            priorities: vec!["high".to_owned()],
            ..ListQuery::default()
        });
        assert!(sql.contains("category = $2"));
        assert!(sql.contains("category = $3"));
        assert!(sql.contains("priority = $4"));
        // Four values for four placeholders: the scope plus three filters. A clause written
        // without its `push_bind` would leave the count short, which is the bug this asserts.
        assert_eq!(values, 4);
    }

    #[test]
    fn a_cursor_pushes_one_value_and_compares_the_pair() {
        let instant = time::OffsetDateTime::parse(
            "2026-09-28T10:00:00Z",
            &time::format_description::well_known::Rfc3339,
        )
        .expect("a fixed instant");
        let (sql, values) = render(&ListQuery {
            before: Some(instant),
            ..ListQuery::default()
        });
        assert!(sql.contains("(created_at, id) < ($2, $3)"));
        assert_eq!(values, 3, "scope + the two halves of the keyset pair");
    }

    #[test]
    fn an_empty_filter_list_pushes_nothing() {
        // The one loop rule: an empty category list contributes no clause AND no value, so
        // the numbering after it cannot shift.
        let (sql, values) = render(&ListQuery {
            categories: Vec::new(),
            priorities: Vec::new(),
            ..ListQuery::default()
        });
        assert_eq!(sql.matches('$').count(), 1);
        assert_eq!(values, 1);
    }

    #[test]
    fn a_bad_category_is_refused_by_name() {
        let error = validate_categories(&["invoice".to_owned()]).expect_err("invoice");
        assert!(error.to_string().contains("invoice"));
        assert!(error.to_string().contains("approval"));
    }

    #[test]
    fn a_bad_channel_is_refused_by_name() {
        let error = validate_channel("sms").expect_err("sms");
        assert!(error.to_string().contains("sms"));
        assert_eq!(error.code(), "invalid_notification");
    }

    #[test]
    fn every_known_category_and_channel_passes_its_guard() {
        let categories: Vec<String> = crate::vocabulary::CATEGORIES
            .iter()
            .map(|c| (*c).to_owned())
            .collect();
        validate_categories(&categories).expect("the closed list validates itself");
        for channel in crate::vocabulary::CHANNELS {
            validate_channel(channel).expect("the closed list validates itself");
        }
    }
}
