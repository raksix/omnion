//! Scheduled publishing (REQ-064, slice 1).
//!
//! A queue row is a promise: *at this instant, this page's lifecycle changes*. The whole module
//! is about keeping that promise exactly once, because the two obvious implementations both
//! fail in ways nobody notices until later.
//!
//! * **A runner that reads due rows and then writes them double-publishes.** Two workers, two
//!   sweeps, one page: both read `status = 'pending'`, both publish, and the queue reports one
//!   `done` and one lost publish. The claim is therefore the write: [`claim_due`] takes the rows
//!   with `for update skip locked` and stamps `claimed_at` *inside* the same transaction that
//!   hands them back, so a second worker skips what the first is holding instead of competing
//!   for it.
//! * **A promise that cannot fail visibly is a promise nobody trusts.** Every attempt records
//!   `result` or `error`; a failed publish stays `failed` with the reason attached and is
//!   reschedulable, because "it silently did not happen" is the failure mode of every queue
//!   that only has a `done` column.
//! * **The timezone is stored beside the instant, not instead of it.** `scheduled_at` is UTC —
//!   the only thing a database can compare — and `timezone` is the author's wall clock, so the
//!   queue screen can print "09:00 in Europe/Istanbul" next to a stored value that a colleague
//!   in another zone reads correctly. Scheduling at an instant with no zone attached is how a
//!   post goes out at 02:00 because the author's laptop was on summer time and the server was
//!   not.

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{ContentError, Result};
use crate::pages;

// The page's *title* is not a column of `pages` — it belongs to the revision (docs/05-VERSIONING
// §4), and there are up to three of them per page. So the join projects the published revision's
// title, falls back to the draft's, and finally to the slug: a queue row is always readable,
// even for a page whose revisions were all trimmed. Selecting `p.title` instead is the mistake
// this comment exists to prevent, because `pages` looks like it should have one.
const PAGE_TITLE: &str = "coalesce( \
     (select title from page_revisions r \
      where r.page_id = p.id and r.state = 'published' order by r.revision_no desc limit 1), \
     (select title from page_revisions r \
      where r.page_id = p.id and r.state = 'draft' order by r.revision_no desc limit 1), \
     p.slug) as page_title";

const QUEUE_COLUMNS: &str = "q.id, q.organization_id, q.page_id, q.action, q.scheduled_at, \
     q.timezone, q.status, q.result, q.error, q.created_by, q.claimed_at, q.created_at, \
     q.updated_at, p.slug as page_slug, p.page_type as page_type, ";

/// The lifecycle changes a schedule may request.
pub const ACTIONS: [&str; 2] = ["publish", "unpublish"];

/// A queue row, joined with the page it acts on.
///
/// The page's slug, type and title ride the row because the queue screen is a list of *pages*,
/// not a list of ids: a row that shows a uuid and requires a second request per row is a screen
/// that cannot be read at a glance, and the join is one indexed lookup on `pages_pkey`.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct PublishingEntry {
    /// Primary key.
    pub id: Uuid,
    /// Organization, for scoping the list.
    pub organization_id: Option<Uuid>,
    /// Page the entry acts on.
    pub page_id: Uuid,
    /// `publish` or `unpublish`.
    pub action: String,
    /// The instant the entry becomes due, in UTC.
    pub scheduled_at: time::OffsetDateTime,
    /// The author's timezone label, shown beside the instant.
    pub timezone: String,
    /// `pending`, `done`, `failed` or `cancelled`.
    pub status: String,
    /// What the last attempt did, in one line.
    pub result: String,
    /// Why the last attempt failed; empty when it did not.
    pub error: String,
    /// Who scheduled it.
    pub created_by: Option<Uuid>,
    /// When a worker took it.
    pub claimed_at: Option<time::OffsetDateTime>,
    /// Creation timestamp.
    pub created_at: time::OffsetDateTime,
    /// Last change.
    pub updated_at: time::OffsetDateTime,
    /// The page's slug, for the screen.
    pub page_slug: String,
    /// The page's content type, for the filter and the label.
    pub page_type: String,
    /// The published (or drafted) title, for the screen.
    pub page_title: String,
}

/// Which entries a list read returns.
#[derive(Debug, Clone, Default)]
pub struct QueueQuery {
    /// Keep only this status; `None` returns pending plus the recent history.
    pub status: Option<String>,
    /// Keep only this content type.
    pub page_type: Option<String>,
    /// Most rows to return.
    pub limit: Option<i64>,
    /// Keep only the sites of this organization. `None` means every site in it, which is what a
    /// caller that named no site asked for.
    pub site_ids: Option<Vec<Uuid>>,
}

/// A schedule to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSchedule {
    /// Page to act on.
    pub page_id: Uuid,
    /// `publish` or `unpublish`.
    pub action: String,
    /// The instant, in UTC.
    pub scheduled_at: time::OffsetDateTime,
    /// The author's timezone label.
    pub timezone: String,
    /// Who scheduled it.
    pub created_by: Option<Uuid>,
}

fn validate_action(action: &str) -> Result<String> {
    let action = action.trim().to_lowercase();
    if ACTIONS.contains(&action.as_str()) {
        Ok(action)
    } else {
        Err(ContentError::InvalidPublishAction(format!(
            "action {action:?} must be one of {}",
            ACTIONS.join(", ")
        )))
    }
}

/// The queue for one organization, soonest first.
///
/// Pending rows come first because the question the screen answers is "what is about to happen",
/// and a queue where the next publish is below forty finished rows has buried its own answer.
pub async fn list_queue(
    pool: &PgPool,
    organization_id: Uuid,
    query: &QueueQuery,
) -> Result<Vec<PublishingEntry>> {
    let status = match query.status.as_deref() {
        Some(status) => Some(validate_queue_status(status)?),
        None => None,
    };
    let page_type = match query.page_type.as_deref() {
        Some(page_type) => Some(
            crate::validation::validate_page_type(page_type)
                .map_err(|_| ContentError::InvalidPublishAction("page type".to_owned()))?,
        ),
        None => None,
    };
    let limit = query.limit.unwrap_or(200).clamp(1, 1_000);
    // The site filter is a LIST, not a single id: the panel is scoped to the site the editor is
    // looking at, and a queue screen that quietly showed a second site's rows would be a leak
    // that looks like a feature. An empty list is "no sites", not "every site" — the caller that
    // means every site sends `None`, so the two can never be confused.
    let site_ids = query.site_ids.as_deref();
    let no_sites = site_ids.is_some_and(<[Uuid]>::is_empty);
    if no_sites {
        return Ok(Vec::new());
    }

    let sql = format!(
        "select {QUEUE_COLUMNS}{PAGE_TITLE} from cms_publishing_queue q \
         join pages p on p.id = q.page_id \
         where q.organization_id = $1 \
           and ($2::text is null or q.status = $2) \
           and ($3::text is null or p.page_type = $3) \
           and ($4::uuid[] is null or p.site_id = any($4)) \
         order by (q.status = 'pending') desc, q.scheduled_at asc, q.id asc \
         limit {limit}"
    );
    sqlx::query_as::<_, PublishingEntry>(&sql)
        .bind(organization_id)
        .bind(status)
        .bind(page_type)
        .bind(site_ids)
        .fetch_all(pool)
        .await
        .map_err(Into::into)
}

/// One entry of an organization.
pub async fn find_entry(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
) -> Result<Option<PublishingEntry>> {
    let sql = format!(
        "select {QUEUE_COLUMNS}{PAGE_TITLE} from cms_publishing_queue q \
         join pages p on p.id = q.page_id \
         where q.organization_id = $1 and q.id = $2"
    );
    sqlx::query_as::<_, PublishingEntry>(&sql)
        .bind(organization_id)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

/// The pending entry for a page and action, if one exists.
pub async fn find_pending(
    pool: &PgPool,
    page_id: Uuid,
    action: &str,
) -> Result<Option<PublishingEntry>> {
    let action = validate_action(action)?;
    let sql = format!(
        "select {QUEUE_COLUMNS}{PAGE_TITLE} from cms_publishing_queue q \
         join pages p on p.id = q.page_id \
         where q.page_id = $1 and q.action = $2 and q.status = 'pending'"
    );
    sqlx::query_as::<_, PublishingEntry>(&sql)
        .bind(page_id)
        .bind(&action)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

/// Schedule a publish or unpublish, replacing any pending entry of the same action.
///
/// Replacing rather than appending is the whole of "reschedule", and the partial unique index
/// `(page_id, action) where status = 'pending'` is what makes the two a single write: a second
/// `POST` for a page that already has one is an update, so a page can never show two "scheduled"
/// lines and never be published twice by two entries.
pub async fn schedule(
    pool: &PgPool,
    organization_id: Uuid,
    new: NewSchedule,
) -> Result<PublishingEntry> {
    let action = validate_action(&new.action)?;
    let timezone = new.timezone.trim();
    if timezone.is_empty() || timezone.len() > 64 {
        return Err(ContentError::InvalidSchedule(
            "the timezone label must be 1-64 characters".to_owned(),
        ));
    }
    // A schedule in the past is refused rather than fired immediately: an author who typed
    // yesterday's date made a typo, and firing it anyway is the surprise version of a helpful
    // assistant. `now - 1 minute` is the grace, so a client whose clock runs a few seconds fast
    // is not punished for it.
    if new.scheduled_at < time::OffsetDateTime::now_utc() - time::Duration::minutes(1) {
        return Err(ContentError::InvalidSchedule(
            "the schedule must be in the future".to_owned(),
        ));
    }

    let sql = format!(
        "insert into cms_publishing_queue \
         (organization_id, page_id, action, scheduled_at, timezone, created_by) \
         values ($1, $2, $3, $4, $5, $6) \
         on conflict (page_id, action) where status = 'pending' do update set \
           scheduled_at = excluded.scheduled_at, timezone = excluded.timezone, \
           created_by = excluded.created_by, result = '', error = '', updated_at = now() \
         returning id"
    );
    let row: (Uuid,) = sqlx::query_as(&sql)
        .bind(organization_id)
        .bind(new.page_id)
        .bind(&action)
        .bind(new.scheduled_at)
        .bind(timezone)
        .bind(new.created_by)
        .fetch_one(pool)
        .await?;

    find_entry(pool, organization_id, row.0)
        .await?
        .ok_or(ContentError::PublishingEntryNotFound)
}

/// Move a pending entry to a new instant. Only pending rows can be rescheduled: a done or
/// failed entry is history, and rewriting its instant would make the queue lie about when
/// something actually happened.
pub async fn reschedule(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    scheduled_at: time::OffsetDateTime,
) -> Result<PublishingEntry> {
    if scheduled_at < time::OffsetDateTime::now_utc() - time::Duration::minutes(1) {
        return Err(ContentError::InvalidSchedule(
            "the schedule must be in the future".to_owned(),
        ));
    }
    let sql = format!(
        "update cms_publishing_queue set scheduled_at = $3, updated_at = now() \
         where id = $1 and organization_id = $2 and status = 'pending' returning id"
    );
    let row: Option<(Uuid,)> = sqlx::query_as(&sql)
        .bind(id)
        .bind(organization_id)
        .bind(scheduled_at)
        .fetch_optional(pool)
        .await?;
    if row.is_none() {
        return Err(ContentError::PublishingEntryNotFound);
    }
    find_entry(pool, organization_id, id)
        .await?
        .ok_or(ContentError::PublishingEntryNotFound)
}

/// Cancel a pending entry. A cancelled entry is kept, not deleted: the queue is the record of
/// what was going to happen, and a row that vanished leaves "did we publish or cancel?" with no
/// answer in the data.
pub async fn cancel(pool: &PgPool, organization_id: Uuid, id: Uuid) -> Result<PublishingEntry> {
    let sql = format!(
        "update cms_publishing_queue \
         set status = 'cancelled', result = 'cancelled by an editor', updated_at = now() \
         where id = $1 and organization_id = $2 and status = 'pending' returning id"
    );
    let row: Option<(Uuid,)> = sqlx::query_as(&sql)
        .bind(id)
        .bind(organization_id)
        .fetch_optional(pool)
        .await?;
    if row.is_none() {
        return Err(ContentError::PublishingEntryNotFound);
    }
    find_entry(pool, organization_id, id)
        .await?
        .ok_or(ContentError::PublishingEntryNotFound)
}

/// Mark a pending entry as due now, so the next sweep claims it.
///
/// The entry is *not* run here: the runner is the one place that publishes, and a "publish now"
/// button that ran a second, lighter copy of that code would produce two definitions of what
/// "published" means. The button moves the instant into the past and the runner does the rest —
/// which is also why the result line on the row is written by the same code either way.
pub async fn publish_now(pool: &PgPool, id: Uuid) -> Result<bool> {
    let updated = sqlx::query(
        "update cms_publishing_queue set scheduled_at = now() - interval '1 second', \
           updated_at = now() \
         where id = $1 and status = 'pending'",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(updated.rows_affected() > 0)
}

/// Take the entries that are due, up to `limit`, and mark them claimed.
///
/// `for update skip locked` is the whole function. It is what makes two workers safe: the second
/// one's `skip locked` steps over the rows the first is holding rather than blocking on them, so
/// a stalled worker cannot become a queue that stops, and a healthy one cannot publish twice.
pub async fn claim_due(
    pool: &PgPool,
    now: time::OffsetDateTime,
    limit: i64,
) -> Result<Vec<PublishingEntry>> {
    let limit = limit.clamp(1, 500);
    let mut tx = pool.begin().await?;
    let sql = format!(
        "with due as ( \
           select id from cms_publishing_queue \
           where status = 'pending' and scheduled_at <= $1 \
           order by scheduled_at asc, id asc \
           for update skip locked \
           limit {limit} \
         ) \
         update cms_publishing_queue q set claimed_at = $1, updated_at = $1 \
         from due where q.id = due.id returning q.id"
    );
    let ids: Vec<(Uuid,)> = sqlx::query_as(&sql).bind(now).fetch_all(&mut *tx).await?;
    if ids.is_empty() {
        tx.commit().await?;
        return Ok(Vec::new());
    }
    let read = format!(
        "select {QUEUE_COLUMNS}{PAGE_TITLE} from cms_publishing_queue q \
         join pages p on p.id = q.page_id \
         where q.id = any($1) order by q.scheduled_at asc, q.id asc"
    );
    let rows = sqlx::query_as::<_, PublishingEntry>(&read)
        .bind(ids.iter().map(|(id,)| *id).collect::<Vec<Uuid>>())
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(rows)
}

/// Claim one specific entry, by id — the `Publish now` path.
///
/// It is [`claim_due`] with a predicate, and it shares the safety property: the row is stamped
/// inside the transaction that hands it back, so a button pressed at the same moment the
/// scheduler fires gets one publish, not two. It is a separate function rather than
/// `claim_due(pool, far_future, 1)` because "the next due entry" and "this entry" are different
/// questions and a button must not publish somebody else's page.
pub async fn claim_entry(pool: &PgPool, id: Uuid) -> Result<Option<PublishingEntry>> {
    let mut tx = pool.begin().await?;
    let claimed = sqlx::query(
        "update cms_publishing_queue set claimed_at = now(), updated_at = now() \
         where id = $1 and status = 'pending' returning id",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    if claimed.rows_affected() == 0 {
        tx.commit().await?;
        return Ok(None);
    }
    let read = format!(
        "select {QUEUE_COLUMNS}{PAGE_TITLE} from cms_publishing_queue q \
         join pages p on p.id = q.page_id where q.id = $1"
    );
    let row = sqlx::query_as::<_, PublishingEntry>(&read)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(row)
}

/// Record the outcome of a claimed entry.
///
/// The `pending` guard is deliberate: a retry that runs twice must not turn a `done` row back
/// into a `failed` one, so the second attempt is a no-op rather than a correction.
pub async fn finish(pool: &PgPool, id: Uuid, result: &str, error: &str) -> Result<()> {
    let status = if error.is_empty() { "done" } else { "failed" };
    sqlx::query(
        "update cms_publishing_queue set status = $2, result = $3, error = $4, updated_at = now() \
         where id = $1 and status = 'pending'",
    )
    .bind(id)
    .bind(status)
    .bind(result)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

/// Put a failed entry back in the queue at a new instant — the queue screen's `Retry`.
pub async fn retry(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    scheduled_at: time::OffsetDateTime,
) -> Result<PublishingEntry> {
    let sql = format!(
        "update cms_publishing_queue \
         set status = 'pending', scheduled_at = $3, error = '', result = '', \
             claimed_at = null, updated_at = now() \
         where id = $1 and organization_id = $2 and status = 'failed' returning id"
    );
    let row: Option<(Uuid,)> = sqlx::query_as(&sql)
        .bind(id)
        .bind(organization_id)
        .bind(scheduled_at)
        .fetch_optional(pool)
        .await?;
    if row.is_none() {
        return Err(ContentError::PublishingEntryNotFound);
    }
    find_entry(pool, organization_id, id)
        .await?
        .ok_or(ContentError::PublishingEntryNotFound)
}

/// Run one claimed entry: publish or unpublish the page, and record what happened.
///
/// The page's own lifecycle functions are the ones that run, so a scheduled publish is
/// indistinguishable from a manual one afterwards — same revision published, same page status.
/// A scheduling feature that wrote its own lighter-weight publish would leave two definitions of
/// "published" in one platform, and they would disagree within a week.
pub async fn run_entry(pool: &PgPool, entry: &PublishingEntry) -> Result<String> {
    let outcome = match entry.action.as_str() {
        "publish" => match pages::publish_page(pool, entry.page_id).await {
            Ok(_) => Ok(format!("published revision for /{}", entry.page_slug)),
            Err(error) => Err(error.to_string()),
        },
        "unpublish" => match pages::unpublish_page(pool, entry.page_id).await {
            // `false` means the page was not published — the state the entry asked for is
            // already true, so it is a *result*, not a failure. Reporting it as an error would
            // leave a red row in the queue for work that is finished.
            Ok(true) => Ok(format!("unpublished /{}", entry.page_slug)),
            Ok(false) => Ok(format!("/{} was not published", entry.page_slug)),
            Err(error) => Err(error.to_string()),
        },
        other => Err(format!("unknown action {other:?}")),
    };
    match outcome {
        Ok(result) => {
            finish(pool, entry.id, &result, "").await?;
            Ok(result)
        }
        Err(error) => {
            finish(pool, entry.id, "", &error).await?;
            Err(ContentError::InvalidSchedule(error))
        }
    }
}

fn validate_queue_status(status: &str) -> Result<String> {
    let status = status.trim().to_lowercase();
    if ["pending", "done", "failed", "cancelled"].contains(&status.as_str()) {
        Ok(status)
    } else {
        Err(ContentError::InvalidStatus(format!(
            "queue status {status:?} must be one of pending, done, failed, cancelled"
        )))
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_action_names_the_legal_values() {
        let error = validate_action("archive").unwrap_err();
        assert_eq!(error.code(), "invalid_publish_action");
        assert!(
            error.to_string().contains("publish, unpublish"),
            "the error must name the legal values: {error}"
        );
    }

    #[test]
    fn an_unknown_status_names_the_legal_values() {
        assert_eq!(
            validate_queue_status("firing").unwrap_err().code(),
            "invalid_status"
        );
        for status in ["pending", "done", "failed", "cancelled"] {
            assert_eq!(validate_queue_status(status).unwrap(), status);
        }
    }

    #[test]
    fn a_schedule_in_the_past_is_refused_with_a_message_that_says_why() {
        let error = ContentError::InvalidSchedule("the schedule must be in the future".to_owned());
        assert!(
            error.to_string().contains("future"),
            "the message must name the problem, not just the field"
        );
    }
}
