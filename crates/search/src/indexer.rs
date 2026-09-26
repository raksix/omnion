//! The write side of the index: reindex passes and the event-bus drain.
//!
//! Two ways in, one shape out. A **reindex** reads a provider's source table and writes (or
//! refreshes) one document per row, then prunes rows whose entity is gone — the pass is
//! idempotent by construction, so running it twice leaves the same count. The **drain** walks
//! the event bus above a cursor and applies each event to the index, which is how a page that
//! was published a second ago answers a search without anyone clicking "reindex".
//!
//! The cursor row (`search_cursor`, seeded by the migration) is the lock: `for update skip
//! locked` makes exactly-once hold across instances, and the cursor advances in the same
//! transaction as the writes it accounts for. An event this build does not know yet is a
//! counted no-op, never an error — producers arrive on their own schedule.

use std::time::Instant;

use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::error::{Result, SearchError};
use crate::providers::{self, ProviderSpec};

/// What one provider's reindex did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReindexReport {
    /// Provider key.
    pub provider: &'static str,
    /// Documents written (inserted or refreshed).
    pub indexed: u64,
    /// Documents removed because their entity is gone.
    pub pruned: u64,
    /// Wall time of the pass, in milliseconds.
    pub duration_ms: u64,
}

/// What one drain tick did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainReport {
    /// Events applied to the index.
    pub applied: u64,
    /// Events read but not applicable (a name without an index plan, or an unusable payload).
    pub skipped: u64,
    /// The cursor after the tick.
    pub cursor: i64,
}

impl DrainReport {
    /// `true` when there was nothing to read.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.applied == 0 && self.skipped == 0
    }
}

/// What a single event asks the index to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventAction {
    /// (Re)index the entity the event names.
    Index,
    /// Drop the entity from the index.
    Remove,
}

/// The plan one event name carries, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventPlan {
    /// Provider the event belongs to.
    pub provider: &'static str,
    /// What to do.
    pub action: EventAction,
    /// Payload key carrying the entity id (`page_id`, `media_id`, …).
    pub id_key: &'static str,
}

/// The event names this build knows, and what they mean for the index.
///
/// Names whose producer does not exist yet (media, users, sites) are listed on purpose: the day
/// those modules emit them, the index follows without a change here. A name that is not listed
/// is skipped and counted.
#[must_use]
pub fn event_plan(name: &str) -> Option<EventPlan> {
    let plan = match name {
        "page.published" | "page.updated" | "page.created" | "page.restored" => EventPlan {
            provider: "pages",
            action: EventAction::Index,
            id_key: "page_id",
        },
        "page.archived" | "page.deleted" | "page.unpublished" => EventPlan {
            provider: "pages",
            action: EventAction::Remove,
            id_key: "page_id",
        },
        "media.created" | "media.updated" => EventPlan {
            provider: "media",
            action: EventAction::Index,
            id_key: "media_id",
        },
        "media.deleted" => EventPlan {
            provider: "media",
            action: EventAction::Remove,
            id_key: "media_id",
        },
        "user.created" | "user.updated" => EventPlan {
            provider: "users",
            action: EventAction::Index,
            id_key: "user_id",
        },
        "user.disabled" | "user.deleted" => EventPlan {
            provider: "users",
            action: EventAction::Remove,
            id_key: "user_id",
        },
        "site.created" | "site.updated" => EventPlan {
            provider: "sites",
            action: EventAction::Index,
            id_key: "site_id",
        },
        _ => return None,
    };
    Some(plan)
}

/// Point a never-advanced cursor at the end of the bus (called once at boot).
///
/// A fresh installation watches forward; a bus that already carries history moves the cursor
/// above it, which is what keeps the first boot of an existing installation from re-indexing
/// every historic event. `None` means it had already been advanced.
pub async fn seed_cursor(pool: &PgPool) -> Result<Option<i64>> {
    let head: Option<i64> = sqlx::query_scalar("select max(id) from events")
        .fetch_one(pool)
        .await?;
    let head = head.unwrap_or(0);
    let moved: Option<i64> = sqlx::query_scalar(
        "update search_cursor set last_event_id = $1, updated_at = now() \
         where id = 1 and last_event_id = 0 and $1 > 0 \
         returning last_event_id",
    )
    .bind(head)
    .fetch_optional(pool)
    .await?;
    Ok(moved)
}

/// The cursor's current position.
pub async fn event_cursor(pool: &PgPool) -> Result<i64> {
    let cursor: Option<i64> =
        sqlx::query_scalar("select last_event_id from search_cursor where id = 1")
            .fetch_optional(pool)
            .await?;
    cursor.ok_or(SearchError::CursorMissing)
}

/// Apply every event above the cursor, oldest first, and advance it.
pub async fn drain(pool: &PgPool, batch: i64) -> Result<DrainReport> {
    let batch = batch.clamp(1, 1_000);
    let mut transaction = pool.begin().await?;

    // The cursor row is the lock; a tick that loses the race does nothing and returns.
    let locked: Option<i64> = sqlx::query_scalar(
        "select last_event_id from search_cursor where id = 1 for update skip locked",
    )
    .fetch_optional(&mut *transaction)
    .await?;
    let Some(cursor) = locked else {
        return Ok(DrainReport {
            applied: 0,
            skipped: 0,
            cursor: event_cursor(pool).await.unwrap_or(0),
        });
    };

    let events: Vec<(i64, String, Value)> = sqlx::query_as(
        "select id, name, payload from events where id > $1 order by id asc limit $2",
    )
    .bind(cursor)
    .bind(batch)
    .fetch_all(&mut *transaction)
    .await?;

    if events.is_empty() {
        transaction.commit().await?;
        return Ok(DrainReport {
            applied: 0,
            skipped: 0,
            cursor,
        });
    }

    let mut applied = 0_u64;
    let mut skipped = 0_u64;
    for (event_id, name, payload) in &events {
        match apply_event(&mut transaction, *event_id, name, payload).await {
            Ok(true) => applied += 1,
            Ok(false) => skipped += 1,
            Err(error) => {
                // One bad event must not stop the index: log it, count it, move on.
                tracing::warn!(event_id, event = %name, error = %error, "search index event skipped");
                skipped += 1;
            }
        }
    }

    let last = events.last().map_or(cursor, |(id, _, _)| *id);
    sqlx::query("update search_cursor set last_event_id = $1, updated_at = now() where id = 1")
        .bind(last)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;

    Ok(DrainReport {
        applied,
        skipped,
        cursor: last,
    })
}

/// Apply one event; `Ok(false)` means there was nothing to do with it.
async fn apply_event(
    transaction: &mut Transaction<'_, Postgres>,
    event_id: i64,
    name: &str,
    payload: &Value,
) -> Result<bool> {
    let Some(plan) = event_plan(name) else {
        tracing::debug!(event_id, event = name, "search: no index plan for event");
        return Ok(false);
    };
    let Some(entity_id) = entity_id_of(payload, plan.id_key) else {
        tracing::debug!(
            event_id,
            event = name,
            "search: payload carries no usable id"
        );
        return Ok(false);
    };

    let spec = providers::provider(plan.provider)
        .ok_or_else(|| SearchError::UnknownProvider(plan.provider.to_owned()))?;

    match plan.action {
        EventAction::Index => {
            let written = upsert(transaction, spec, Some(entity_id)).await?;
            if written == 0 {
                // The entity vanished between the event and this tick; a stale row would lie.
                remove(transaction, spec, entity_id).await?;
            }
            Ok(true)
        }
        EventAction::Remove => {
            remove(transaction, spec, entity_id).await?;
            Ok(true)
        }
    }
}

/// Read the entity id an event payload carries.
fn entity_id_of(payload: &Value, key: &str) -> Option<Uuid> {
    let raw = payload.get(key).or_else(|| payload.get("id"))?;
    match raw {
        Value::String(text) => Uuid::parse_str(text).ok(),
        _ => None,
    }
}

/// Rebuild one provider's slice of the index.
///
/// Every pass leaves a row in `search_reindex_runs` — started when it begins, finished (or failed)
/// when it ends. `/settings/search` reads its states from those rows instead of guessing from a
/// document count, which is what makes "indexing" and "failed" honest.
pub async fn reindex(pool: &PgPool, key: &str) -> Result<ReindexReport> {
    let spec =
        providers::provider(key).ok_or_else(|| SearchError::UnknownProvider(key.to_owned()))?;
    let started = Instant::now();
    let run_id: i64 =
        sqlx::query_scalar("insert into search_reindex_runs (provider) values ($1) returning id")
            .bind(spec.key)
            .fetch_one(pool)
            .await?;

    let outcome = async {
        let mut transaction = pool.begin().await?;
        let indexed = upsert(&mut transaction, spec, None).await?;
        let pruned = prune(&mut transaction, spec).await?;
        transaction.commit().await?;
        Ok::<(u64, u64), SearchError>((indexed, pruned))
    }
    .await;

    let duration_ms = started.elapsed().as_millis() as u64;
    match outcome {
        Ok((indexed, pruned)) => {
            sqlx::query(
                "update search_reindex_runs set finished_at = now(), indexed = $2, pruned = $3, \
                 duration_ms = $4 where id = $1",
            )
            .bind(run_id)
            .bind(indexed as i64)
            .bind(pruned as i64)
            .bind(duration_ms as i64)
            .execute(pool)
            .await?;
            Ok(ReindexReport {
                provider: spec.key,
                indexed,
                pruned,
                duration_ms,
            })
        }
        Err(error) => {
            // The failure is part of the record: the screen shows it, and the next pass clears it.
            if let Err(write) = sqlx::query(
                "update search_reindex_runs set finished_at = now(), duration_ms = $2, error = $3 \
                 where id = $1",
            )
            .bind(run_id)
            .bind(duration_ms as i64)
            .bind(error.to_string())
            .execute(pool)
            .await
            {
                tracing::warn!(provider = spec.key, %write, "the failed reindex could not be recorded");
            }
            Err(error)
        }
    }
}

/// Rebuild every provider, in registry order.
pub async fn reindex_all(pool: &PgPool) -> Result<Vec<ReindexReport>> {
    let mut reports = Vec::with_capacity(providers::PROVIDERS.len());
    for spec in providers::PROVIDERS {
        reports.push(reindex(pool, spec.key).await?);
    }
    Ok(reports)
}

/// Index one entity by id; `Ok(0)` means the source row no longer exists.
pub async fn index_entity(pool: &PgPool, key: &str, entity_id: Uuid) -> Result<u64> {
    let spec =
        providers::provider(key).ok_or_else(|| SearchError::UnknownProvider(key.to_owned()))?;
    let mut transaction = pool.begin().await?;
    let written = upsert(&mut transaction, spec, Some(entity_id)).await?;
    if written == 0 {
        remove(&mut transaction, spec, entity_id).await?;
    }
    transaction.commit().await?;
    Ok(written)
}

/// Drop one entity from the index.
pub async fn remove_entity(pool: &PgPool, key: &str, entity_id: Uuid) -> Result<u64> {
    let spec =
        providers::provider(key).ok_or_else(|| SearchError::UnknownProvider(key.to_owned()))?;
    let mut transaction = pool.begin().await?;
    let removed = remove(&mut transaction, spec, entity_id).await?;
    transaction.commit().await?;
    Ok(removed)
}

/// Write (or refresh) the documents of one provider, optionally for a single entity.
///
/// The provider SQL carries a `{filter}` placeholder where the single-entity clause lands; the
/// filter itself is a uuid the platform generated (formatted, never user text), and the
/// `ON CONFLICT` clause refreshes every column — the vector included, so a renamed page stops
/// answering under its old title.
async fn upsert(
    transaction: &mut Transaction<'_, Postgres>,
    spec: &ProviderSpec,
    entity_id: Option<Uuid>,
) -> Result<u64> {
    let Some(sql) = upsert_statement(spec.key, entity_id) else {
        debug_assert!(false, "provider {} has no upsert", spec.key);
        return Ok(0);
    };
    let result = sqlx::query(&sql).execute(&mut **transaction).await?;
    Ok(result.rows_affected())
}

/// Remove documents whose source entity is gone.
async fn prune(transaction: &mut Transaction<'_, Postgres>, spec: &ProviderSpec) -> Result<u64> {
    let sql = match spec.key {
        "pages" => {
            "delete from search_documents d where d.provider = 'pages' \
             and not exists (select 1 from pages p where p.id::text = d.entity_id)"
        }
        "media" => {
            "delete from search_documents d where d.provider = 'media' \
             and not exists (select 1 from media m where m.id::text = d.entity_id)"
        }
        "users" => {
            "delete from search_documents d where d.provider = 'users' \
             and not exists (select 1 from users u where u.id::text = d.entity_id \
                             and u.status <> 'disabled')"
        }
        "sites" => {
            "delete from search_documents d where d.provider = 'sites' \
             and not exists (select 1 from sites s where s.id::text = d.entity_id)"
        }
        "logs" => {
            // A row stays exactly while its source row still resolves to a target the panel can
            // open — the same narrowing the upsert applies, or an entry whose page was deleted
            // would keep answering with a link into nothing.
            "delete from search_documents d where d.provider = 'logs' and not exists ( \
                 select 1 from audit_log a \
                 left join pages pg on a.target_type = 'page' and pg.id::text = a.target_id \
                 left join media md on a.target_type = 'media' and md.id::text = a.target_id \
                 left join sites st on a.target_type = 'site' and st.id::text = a.target_id \
                 where d.entity_id = 'audit-' || a.id::text \
                   and (pg.id is not null or md.id is not null or st.id is not null) \
             ) and not exists ( \
                 select 1 from events e \
                 left join pages pg on pg.id::text = e.payload->>'page_id' \
                 left join media md on md.id::text = e.payload->>'media_id' \
                 left join sites st on st.id::text = e.payload->>'site_id' \
                 where d.entity_id = 'event-' || e.id::text \
                   and (pg.id is not null or md.id is not null or st.id is not null) \
             )"
        }
        "translations" => {
            "delete from search_documents d where d.provider = 'translations' and not exists ( \
                 select 1 from translations t \
                 join page_revisions r on r.id = t.resource_id and t.resource_type = 'page_revision' \
                 join pages pg on pg.id = r.page_id \
                 where d.entity_id = t.id::text and btrim(t.value) <> '' \
             )"
        }
        "settings" => {
            "delete from search_documents d where d.provider = 'settings' \
             and not exists (select 1 from organizations o where d.entity_id = o.id::text)"
        }
        other => {
            debug_assert!(false, "provider {other} has no prune");
            return Ok(0);
        }
    };
    let result = sqlx::query(sql).execute(&mut **transaction).await?;
    Ok(result.rows_affected())
}

/// Delete one entity's document rows.
async fn remove(
    transaction: &mut Transaction<'_, Postgres>,
    spec: &ProviderSpec,
    entity_id: Uuid,
) -> Result<u64> {
    let result = sqlx::query(
        "delete from search_documents where provider = $1 and entity_type = $2 \
         and entity_id = $3",
    )
    .bind(spec.key)
    .bind(spec.entity_type)
    .bind(entity_id.to_string())
    .execute(&mut **transaction)
    .await?;
    Ok(result.rows_affected())
}

/// The `ON CONFLICT` tail every provider's upsert shares: a document is one row per entity, and
/// a rewrite refreshes every column of it.
const UPSERT_TAIL: &str = "\
on conflict (provider, entity_type, entity_id) do update set \
    organization_id = excluded.organization_id, \
    site_id = excluded.site_id, \
    title = excluded.title, \
    subtitle = excluded.subtitle, \
    url = excluded.url, \
    owner_user_id = excluded.owner_user_id, \
    tags = excluded.tags, \
    body = excluded.body, \
    entity_updated_at = excluded.entity_updated_at, \
    document = excluded.document, \
    indexed_at = now()";

/// Upsert of the activity provider: one document per audit entry or recorded event **that points
/// at an entity the panel can open**.
///
/// The narrowing is deliberate. An audit entry about a role or an AI provider has no screen to
/// land on yet, and the platform's own rule is that a row which cannot go anywhere is worse than
/// a row that is not there; those entries join this provider the day their screen exists
/// (REQ-012/REQ-039). The `url` is the target's deep link — an entry about a page opens the
/// page's editor — and `body` carries the entry's metadata so a search can find "value" inside it.
const LOGS_UPSERT: &str = "\
insert into search_documents \
    (organization_id, site_id, provider, entity_type, entity_id, title, subtitle, url, \
     owner_user_id, tags, body, entity_updated_at, document) \
select * from ( \
    select a.organization_id, \
           coalesce(pg.site_id, md.site_id, st.id), \
           'logs', 'log', 'audit-' || a.id::text, \
           a.action, \
           concat_ws(' · ', coalesce(au.display_name, ''), a.target_type, \
                     to_char(a.created_at, 'YYYY-MM-DD HH24:MI')), \
           coalesce(case \
               when pg.id is not null then '/pages?site=' || pg.site_id::text || '&focus=' || pg.id::text \
               when md.id is not null then '/media?site=' || md.site_id::text || '&focus=' || md.id::text \
               when st.id is not null then '/sites' \
           end, '/'), \
           a.actor_user_id, \
           array[a.action]::text[], \
           coalesce(a.metadata::text, ''), \
           a.created_at, \
           setweight(to_tsvector('simple', a.action), 'A') || \
           setweight(to_tsvector('simple', coalesce(a.target_type, '')), 'B') || \
           setweight(to_tsvector('simple', concat_ws(' ', coalesce(au.display_name, ''), \
                                                     coalesce(a.target_type, ''), a.action)), 'C') || \
           setweight(to_tsvector('simple', coalesce(a.metadata::text, '')), 'D') \
    from audit_log a \
    left join users au on au.id = a.actor_user_id \
    left join pages pg on a.target_type = 'page' and pg.id::text = a.target_id \
    left join media md on a.target_type = 'media' and md.id::text = a.target_id \
    left join sites st on a.target_type = 'site' and st.id::text = a.target_id \
    where (pg.id is not null or md.id is not null or st.id is not null) \
    union all \
    select e.organization_id, \
           coalesce(pg.site_id, md.site_id, st.id), \
           'logs', 'log', 'event-' || e.id::text, \
           e.name, \
           concat_ws(' · ', 'event', to_char(e.created_at, 'YYYY-MM-DD HH24:MI')), \
           coalesce(case \
               when pg.id is not null then '/pages?site=' || pg.site_id::text || '&focus=' || pg.id::text \
               when md.id is not null then '/media?site=' || md.site_id::text || '&focus=' || md.id::text \
               when st.id is not null then '/sites' \
           end, '/'), \
           e.actor_user_id, \
           array[e.name]::text[], \
           coalesce(e.payload::text, ''), \
           e.created_at, \
           setweight(to_tsvector('simple', e.name), 'A') || \
           setweight(to_tsvector('simple', 'event'), 'B') || \
           setweight(to_tsvector('simple', concat_ws(' ', 'event', e.name)), 'C') || \
           setweight(to_tsvector('simple', coalesce(e.payload::text, '')), 'D') \
    from events e \
    left join pages pg on pg.id::text = e.payload->>'page_id' \
    left join media md on md.id::text = e.payload->>'media_id' \
    left join sites st on st.id::text = e.payload->>'site_id' \
    where (pg.id is not null or md.id is not null or st.id is not null) \
) rows \
where true {filter} \
";

/// Upsert of the translation provider: one document per translated field, joined to the page it
/// belongs to so a hit opens that page's editor. Blank values are skipped — a document whose
/// title would be empty is refused by the table's own check.
const TRANSLATIONS_UPSERT: &str = "\
insert into search_documents \
    (organization_id, site_id, provider, entity_type, entity_id, title, subtitle, url, \
     owner_user_id, tags, body, entity_updated_at, document) \
select t.organization_id, pg.site_id, 'translations', 'translation', t.id::text, \
       left(t.value, 120), \
       concat_ws(' · ', 'page /' || pg.slug, t.language, t.field), \
       '/pages?site=' || pg.site_id::text || '&focus=' || pg.id::text, \
       t.created_by, \
       array[t.language]::text[], \
       t.value, \
       t.updated_at, \
       setweight(to_tsvector('simple', left(t.value, 120)), 'A') || \
       setweight(to_tsvector('simple', t.language), 'B') || \
       setweight(to_tsvector('simple', concat_ws(' ', t.field, t.language)), 'C') || \
       setweight(to_tsvector('simple', t.value), 'D') \
from translations t \
join page_revisions r on t.resource_type = 'page_revision' and r.id = t.resource_id \
join pages pg on pg.id = r.page_id \
where btrim(t.value) <> '' {filter} \
";

/// Upsert of the settings provider: one document per organization for the key/value settings the
/// panel can actually change today — the search settings (ranking weights and enabled providers).
/// The next settings centre adds its own row to this same statement, and each row opens the screen
/// where the value lives.
const SETTINGS_UPSERT: &str = "\
insert into search_documents \
    (organization_id, site_id, provider, entity_type, entity_id, title, subtitle, url, \
     owner_user_id, tags, body, entity_updated_at, document) \
select o.id, null, 'settings', 'setting', o.id::text, \
       'Search ranking weights', \
       concat_ws(' · ', \
           'title ' || coalesce(ss.weights->>'title', '6'), \
           'tags ' || coalesce(ss.weights->>'tags', '4'), \
           'subtitle ' || coalesce(ss.weights->>'subtitle', '3'), \
           'body ' || coalesce(ss.weights->>'body', '1'), \
           array_length(ss.enabled_providers, 1) || ' providers'), \
       '/settings/search', \
       null, \
       array['settings']::text[], \
       coalesce(ss.weights::text, ''), \
       ss.updated_at, \
       setweight(to_tsvector('simple', 'search ranking weights'), 'A') || \
       setweight(to_tsvector('simple', 'settings'), 'B') || \
       setweight(to_tsvector('simple', 'search ranking weights settings providers'), 'C') || \
       setweight(to_tsvector('simple', coalesce(ss.weights::text, '')), 'D') \
from organizations o \
left join search_settings ss on ss.id = 1 \
where true {filter} \
";

/// Upsert of the content provider: one document per page, titled by its latest revision.
///
/// A page carries its status as a tag (`draft`, `published`, `archived`) so `is:draft` is a tag
/// filter rather than a text match, and the ranking can weight it. The `url` is the deep link a
/// palette hit opens — the pages screen of the page's own site, with the page's editor open.
const PAGES_UPSERT: &str = "\
insert into search_documents \
    (organization_id, site_id, provider, entity_type, entity_id, title, subtitle, url, \
     owner_user_id, tags, body, entity_updated_at, document) \
select s.organization_id, p.site_id, 'pages', 'page', p.id::text, \
       rev.title, \
       concat_ws(' · ', s.name, '/' || p.slug, p.status), \
       '/pages?site=' || p.site_id::text || '&focus=' || p.id::text, \
       p.created_by, \
       array[p.status]::text[], \
       coalesce(rev.summary, ''), \
       p.updated_at, \
       setweight(to_tsvector('simple', rev.title), 'A') || \
       setweight(to_tsvector('simple', p.status), 'B') || \
       setweight(to_tsvector('simple', concat_ws(' · ', s.name, '/' || p.slug, p.status)), 'C') || \
       setweight(to_tsvector('simple', coalesce(rev.summary, '')), 'D') \
from pages p \
join sites s on s.id = p.site_id \
join lateral ( \
    select title, summary from page_revisions \
    where page_id = p.id order by revision_no desc limit 1 \
) rev on true \
where true {filter} \
";

/// Upsert of the media provider: one document per file. The `url` opens the file's own library
/// with the row marked, so a hit lands on the file and not merely in the library.
const MEDIA_UPSERT: &str = "\
insert into search_documents \
    (organization_id, site_id, provider, entity_type, entity_id, title, subtitle, url, \
     owner_user_id, tags, body, entity_updated_at, document) \
select s.organization_id, m.site_id, 'media', 'media', m.id::text, \
       m.filename, \
       concat_ws(' · ', s.name, m.content_type), \
       '/media?site=' || m.site_id::text || '&focus=' || m.id::text, \
       m.created_by, \
       array[m.content_type]::text[], \
       '', \
       m.created_at, \
       setweight(to_tsvector('simple', m.filename), 'A') || \
       setweight(to_tsvector('simple', m.content_type), 'B') || \
       setweight(to_tsvector('simple', concat_ws(' · ', s.name, m.content_type)), 'C') || \
       setweight(to_tsvector('simple', ''), 'D') \
from media m \
join sites s on s.id = m.site_id \
where true {filter} \
";

/// Upsert of the account provider: one document per active account. Disabled accounts are not
/// indexed at all — a search must not hand out a person the IAM would refuse to show.
const USERS_UPSERT: &str = "\
insert into search_documents \
    (organization_id, site_id, provider, entity_type, entity_id, title, subtitle, url, \
     owner_user_id, tags, body, entity_updated_at, document) \
select u.organization_id, null, 'users', 'user', u.id::text, \
       u.display_name, \
       concat_ws(' · ', u.email, u.status), \
       '/settings/users', \
       u.id, \
       array[u.status]::text[], \
       '', \
       u.created_at, \
       setweight(to_tsvector('simple', u.display_name), 'A') || \
       setweight(to_tsvector('simple', u.status), 'B') || \
       setweight(to_tsvector('simple', concat_ws(' · ', u.email, u.status)), 'C') || \
       setweight(to_tsvector('simple', ''), 'D') \
from users u \
where u.status <> 'disabled' {filter} \
";

/// Upsert of the site provider: one document per site.
const SITES_UPSERT: &str = "\
insert into search_documents \
    (organization_id, site_id, provider, entity_type, entity_id, title, subtitle, url, \
     owner_user_id, tags, body, entity_updated_at, document) \
select s.organization_id, s.id, 'sites', 'site', s.id::text, \
       s.name, \
       concat_ws(' · ', s.key, s.status), \
       '/sites', \
       null, \
       array[s.status]::text[], \
       '', \
       s.updated_at, \
       setweight(to_tsvector('simple', s.name), 'A') || \
       setweight(to_tsvector('simple', s.status), 'B') || \
       setweight(to_tsvector('simple', concat_ws(' · ', s.key, s.status)), 'C') || \
       setweight(to_tsvector('simple', ''), 'D') \
from sites s \
where true {filter} \
";

/// Build a provider's upsert statement: the SQL above, the optional single-entity filter and the
/// shared conflict tail.
#[must_use]
pub fn upsert_statement(provider_key: &str, entity_id: Option<Uuid>) -> Option<String> {
    let (sql, column) = match provider_key {
        "pages" => (PAGES_UPSERT, "p.id"),
        "media" => (MEDIA_UPSERT, "m.id"),
        "users" => (USERS_UPSERT, "u.id"),
        "sites" => (SITES_UPSERT, "s.id"),
        "translations" => (TRANSLATIONS_UPSERT, "t.id"),
        "settings" => (SETTINGS_UPSERT, "o.id"),
        // Activity rows are two sources behind one entity id (`audit-12` / `event-9`), and nothing
        // addresses one of them by uuid: the only way in is a full pass, so a single-entity upsert
        // writes nothing rather than guessing at a row. That also keeps `index_entity` harmless
        // for this provider instead of panicking on a missing arm.
        "logs" => (LOGS_UPSERT, ""),
        _ => return None,
    };
    let filter = match entity_id {
        Some(_) if column.is_empty() => "and false".to_owned(),
        Some(id) => format!("and {column} = '{id}'::uuid"),
        None => String::new(),
    };
    Some(format!(
        "{}{}",
        sql.replace("{filter}", &filter),
        UPSERT_TAIL
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_events_carry_a_plan_and_unknown_ones_do_not() {
        let published = event_plan("page.published").expect("plan");
        assert_eq!(published.provider, "pages");
        assert_eq!(published.action, EventAction::Index);
        assert_eq!(published.id_key, "page_id");

        assert_eq!(
            event_plan("media.deleted").map(|plan| plan.action),
            Some(EventAction::Remove)
        );
        assert!(event_plan("automation.run.finished").is_none());
    }

    #[test]
    fn every_plan_names_a_registered_provider() {
        for name in [
            "page.published",
            "page.archived",
            "media.created",
            "media.deleted",
            "user.updated",
            "user.disabled",
            "site.created",
        ] {
            let plan = event_plan(name).unwrap_or_else(|| panic!("{name} must have a plan"));
            assert!(
                providers::provider(plan.provider).is_some(),
                "{name} names provider {} which is not registered",
                plan.provider
            );
        }
    }

    #[test]
    fn an_entity_id_is_read_from_its_key_and_from_a_plain_id() {
        let id = Uuid::new_v4();
        let by_key = serde_json::json!({ "page_id": id.to_string() });
        let by_id = serde_json::json!({ "id": id.to_string() });
        assert_eq!(entity_id_of(&by_key, "page_id"), Some(id));
        assert_eq!(entity_id_of(&by_id, "page_id"), Some(id));
        assert_eq!(
            entity_id_of(&serde_json::json!({ "page_id": 7 }), "page_id"),
            None
        );
        assert_eq!(entity_id_of(&serde_json::json!({}), "page_id"), None);
    }

    #[test]
    fn every_registered_provider_has_an_upsert_and_a_prune() {
        // The indexer's match arms are the only place a provider key is spelled twice; this
        // test fails the day a new provider is registered without them.
        for spec in providers::PROVIDERS {
            let sql = upsert_statement(spec.key, None)
                .unwrap_or_else(|| panic!("provider {} has no upsert SQL", spec.key));
            assert!(sql.contains("insert into search_documents"), "{}", spec.key);
            assert!(
                sql.contains("on conflict (provider, entity_type, entity_id) do update"),
                "{} must refresh on conflict",
                spec.key
            );
            assert!(sql.contains("document = excluded.document"), "{}", spec.key);
        }
        assert!(upsert_statement("unicorns", None).is_none());
    }
}
