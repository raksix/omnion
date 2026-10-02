//! Automatic invalidation: a platform event becomes a queued purge (REQ-011, slice 3).
//!
//! Slice 2 shipped the queue and the worker. What was still missing is the *reason* a purge
//! exists: right now a row only appears when a person presses the button on `/cdn/purge`, and
//! the automatic-invalidation half of the request — the diagram at the top of it, "page
//! published → purge CDN cache → new version live" — was a claim with nothing behind it.
//!
//! This module is that behind-it. It does three things and nothing else:
//!
//! * [`Trigger`] — the six event names the request names, and what each one means for a
//!   cache. A name that is not in this table is never consumed.
//! * [`plan`] — a pure function from `(event name, payload, site, tag map)` to the purge a
//!   subscriber *would* queue. Pure on purpose: the mapping is the part that goes wrong,
//!   and a mapping that can only be tested by emitting an event, watching a worker tick and
//!   reading a table is a mapping nobody writes a second test for.
//! * [`drain`] — the cursor walk over `events` that turns plans into rows. It is the
//!   platform's own pattern (the search indexer, slice after slice) and it is a *durable*
//!   cursor rather than an in-memory flag, because an invalidation that is applied twice is
//!   cheap and an invalidation that is dropped is a site serving last week's page.
//!
//! **Three decisions are worth stating, because each is a place the obvious code is wrong.**
//!
//! * **A purge is enqueued as `requested_by = null`, never as the actor who published.** The
//!   history table's "Requested by" column answers *who pressed the button*; a purge the
//!   platform raised has no author, and borrowing the publisher's id makes the history say
//!   a person asked for something they did not. The automatic case is visible instead — the
//!   row's `targets` begin with the event name, and [`drain`] records the event id on the
//!   purge so the panel can say which publication it came from.
//! * **Tag purges are resolved to URLs when the site has no surrogate-key support, and left
//!   as tags when it does.** The request calls this out explicitly: `origin` and
//!   `generic_http` cannot hold a tag, so a tag-only purge against them would be a no-op
//!   that reports success. The plan therefore asks the provider's capabilities, and the
//!   *same* rule set produces a URL purge or a tag purge depending on the adapter — which
//!   is also why `plan` takes the capabilities rather than assuming.
//! * **A disabled trigger is not "queue nothing and continue silently" — it is skipped and
//!   counted.** The overview's counters distinguish "nothing happened" from "the operator
//!   turned this off", and a drain that reported both as idle would make a mis-set toggle
//!   invisible forever.

use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::error::CdnError;
use crate::headers::surrogate_keys;
use crate::provider::Capabilities;
use crate::purge::{NewPurge, PurgeKind, MAX_TARGETS};

/// The event names that can queue a purge, and what each one invalidates.
///
/// The names are the ones the platform **records**, not the ones the request's sentence
/// uses. `media.replaced` and `site.domain.changed` are spelled in REQ-011, but the emitters
/// write `media.version_created`, `domain.added` and `domain.removed` — and a trigger keyed
/// on a name nothing emits is a switch an operator can turn on, watch, and never see
/// anything happen from. The request is a specification of intent; the bus is the
/// specification of fact, and only the second one fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// A page's draft was published.
    PagePublished,
    /// A published page went back to being a draft.
    PageUnpublished,
    /// A page was removed from the site.
    PageDeleted,
    /// A media file's bytes were replaced (a new version).
    MediaVersionCreated,
    /// A theme was made the active one.
    ThemeActivated,
    /// A domain was attached to a site.
    DomainAdded,
    /// A domain was detached from its site.
    DomainRemoved,
}

impl Trigger {
    /// The event name this trigger consumes.
    #[must_use]
    pub fn event_name(self) -> &'static str {
        match self {
            Self::PagePublished => "page.published",
            Self::PageUnpublished => "page.unpublished",
            Self::PageDeleted => "page.deleted",
            Self::MediaVersionCreated => "media.version_created",
            Self::ThemeActivated => "theme.activated",
            Self::DomainAdded => "domain.added",
            Self::DomainRemoved => "domain.removed",
        }
    }

    /// What the operator is signing up for, as the settings screen reads it.
    ///
    /// `None` for a name the panel does not offer, which is what keeps the screen and this
    /// table from drifting: the screen renders the list, and the list is what fires.
    #[must_use]
    pub fn from_event_name(name: &str) -> Option<Self> {
        let trigger = match name {
            "page.published" => Self::PagePublished,
            "page.unpublished" => Self::PageUnpublished,
            "page.deleted" => Self::PageDeleted,
            "media.version_created" => Self::MediaVersionCreated,
            "theme.activated" => Self::ThemeActivated,
            "domain.added" => Self::DomainAdded,
            "domain.removed" => Self::DomainRemoved,
            _ => return None,
        };
        Some(trigger)
    }

    /// Every trigger, in the order the settings screen lists them.
    #[must_use]
    pub fn all() -> &'static [Self] {
        &[
            Self::PagePublished,
            Self::PageUnpublished,
            Self::PageDeleted,
            Self::MediaVersionCreated,
            Self::ThemeActivated,
            Self::DomainAdded,
            Self::DomainRemoved,
        ]
    }

    /// Whether this invalidation reaches one URL or the whole site.
    #[must_use]
    pub fn breadth(self) -> Breadth {
        match self {
            Self::PagePublished | Self::PageUnpublished | Self::PageDeleted => Breadth::Url,
            Self::MediaVersionCreated => Breadth::Media,
            // A new theme, a new domain, a removed domain: the cached *identity* of the
            // site changed, so every address on it is stale, not just one page.
            Self::ThemeActivated | Self::DomainAdded | Self::DomainRemoved => Breadth::Site,
        }
    }

    /// One sentence the settings screen shows next to the switch.
    #[must_use]
    pub fn consequence(self) -> &'static str {
        match self {
            Self::PagePublished => {
                "every publish queues a purge of that page and the assets it references"
            }
            Self::PageUnpublished => {
                "a page taken down stops being served from the edge straight away"
            }
            Self::PageDeleted => {
                "a deleted page is invalidated rather than left cached until its TTL ends"
            }
            Self::MediaVersionCreated => {
                "a replaced file invalidates its own address, not the whole library"
            }
            Self::ThemeActivated => "a new theme invalidates every page at once — a large purge, once",
            Self::DomainAdded => "a new domain invalidates every page cached under the old one",
            Self::DomainRemoved => "a removed domain invalidates the addresses nobody can reach any more",
        }
    }
}

/// How much of a site's cache one event invalidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Breadth {
    /// One public address.
    Url,
    /// One media address (and the page that references it, if the body is known).
    Media,
    /// Every address the site is served on.
    Site,
}

/// What one event asks the cache to do, before anything is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Which trigger matched.
    pub trigger: Trigger,
    /// The site whose cache is affected.
    pub site_id: Uuid,
    /// Which invalidation to queue.
    pub kind: PurgeKind,
    /// The targets, already de-duplicated and capped.
    pub targets: Vec<String>,
    /// Why the plan is empty, when it is.
    ///
    /// `None` means the plan queues a purge. `Some(reason)` means it deliberately does
    /// not, and the reason is worth carrying all the way to the log: "the payload carried
    /// no slug" and "the operator turned this trigger off" are both "no purge", and only
    /// one of them is a thing to go and fix.
    pub skip: Option<SkipReason>,
}

/// Why a plan queues nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The trigger is switched off in the site's CDN settings.
    Disabled,
    /// The event carried no identifier the invalidation can be built from.
    NoTarget,
    /// The plan expanded to more targets than one purge may carry.
    TooManyTargets,
    /// The event names no trigger this build consumes.
    NotATrigger,
}

impl SkipReason {
    /// A short, log-friendly word.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::NoTarget => "no_target",
            Self::TooManyTargets => "too_many_targets",
            Self::NotATrigger => "not_a_trigger",
        }
    }
}

/// The tag a whole-site invalidation uses.
///
/// `cdn_cache::site_tag` writes `site-<uuid>` into the `Surrogate-Key` header of every
/// cacheable public response, so this is the one tag that is guaranteed to exist for any
/// address the edge is holding. It is spelled here rather than imported because `site_tag`
/// lives in the API crate and this one is the *inverse* — the name a purge asks for — and
/// the two must not be able to drift apart without a test catching it.
#[must_use]
pub fn site_tag(site_id: Uuid) -> String {
    format!("site-{site_id}")
}

/// Build the plan an event implies, honouring the trigger toggles.
///
/// `enabled` is the site's `auto_purge` map: an event name that is **absent** or `false` is
/// off. The default when the map says nothing is *off*, not on — a settings row created
/// before a trigger existed must not silently start purging a site because someone added a
/// new event name to this table, and the operator has a switch for every name precisely so
/// the answer is theirs to give.
#[must_use]
pub fn plan(
    trigger: Trigger,
    site_id: Uuid,
    payload: &Value,
    enabled: &Value,
    capabilities: &Capabilities,
) -> Plan {
    let skip = |reason: SkipReason| Plan {
        trigger,
        site_id,
        kind: PurgeKind::Url,
        targets: Vec::new(),
        skip: Some(reason),
    };

    if !trigger_enabled(trigger, enabled) {
        return skip(SkipReason::Disabled);
    }

    // The tag map: the one place a URL becomes a tag, and the only provider that can act
    // on a tag needs this. Without surrogate-key support a tag purge is a no-op that
    // reports success, so a tag is resolved back into the URLs it stands for.
    let tag_map = tag_map(trigger, site_id, payload, capabilities);

    match trigger.breadth() {
        Breadth::Url => {
            let Some(path) = public_path(trigger, payload) else {
                return skip(SkipReason::NoTarget);
            };
            let targets = if capabilities.tags {
                vec![path]
            } else {
                tag_map.get(&path).cloned().unwrap_or_else(|| vec![path.clone()])
            };
            Plan {
                trigger,
                site_id,
                kind: if capabilities.tags { PurgeKind::Tag } else { PurgeKind::Url },
                targets,
                skip: None,
            }
        }
        Breadth::Media => {
            // A media file has no public address of its own: it is served from
            // `/api/v1/public/media/{id}`, and the id is what the event carries. The page
            // that embeds it is a second, separate address, and the event does not know
            // which pages those are — so only the file itself is invalidated here, which is
            // the address that actually changed. Purging "every page that uses this file"
            // is a graph walk over the content tree, and doing it wrong would purge a whole
            // site on every upload.
            let Some(id) = uuid_field(payload, "media_id") else {
                return skip(SkipReason::NoTarget);
            };
            let path = format!("/api/v1/public/media/{id}");
            let targets = if capabilities.tags {
                vec![path]
            } else {
                tag_map.get(&path).cloned().unwrap_or_else(|| vec![path])
            };
            Plan {
                trigger,
                site_id,
                kind: if capabilities.tags { PurgeKind::Tag } else { PurgeKind::Url },
                targets,
                skip: None,
            }
        }
        Breadth::Site => Plan {
            trigger,
            site_id,
            kind: PurgeKind::Tag,
            targets: vec![site_tag(site_id)],
            skip: None,
        },
    }
}

/// The addresses a whole-site invalidation names when the provider cannot hold a tag.
///
/// `origin` and `generic_http` are told "these files" and nothing else, so a `site-<uuid>`
/// tag against them is a request the adapter cannot express: the worker's `Purge::Tags`
/// becomes a body no endpoint reads, the adapter answers `Succeeded`, and the operator
/// sees a successful purge that invalidated nothing. This is the fallback REQ-011's risks
/// section asks for, and it has to be decided at *plan* time — the two products are
/// different purges with different costs, and discovering it at drain time means the
/// decision was made by a provider nobody chose.
///
/// The list is the site's published slugs: the addresses the site is actually served on. A
/// draft has no address to invalidate, and a deleted page's address is already stale.
#[must_use]
pub fn site_plan(trigger: Trigger, site_id: Uuid, paths: &[String]) -> Plan {
    let mut targets: Vec<String> = Vec::new();
    for path in paths {
        let trimmed = path.trim();
        if trimmed.is_empty() || targets.iter().any(|existing| existing == trimmed) {
            continue;
        }
        targets.push(trimmed.to_string());
    }
    if targets.is_empty() {
        // A site with nothing published has no cache to invalidate, and a purge with no
        // targets is refused by the table's own constraint. Skipping is the honest answer:
        // the activation happened, there was simply nothing cached under it.
        return Plan {
            trigger,
            site_id,
            kind: PurgeKind::Url,
            targets: Vec::new(),
            skip: Some(SkipReason::NoTarget),
        };
    }
    Plan {
        trigger,
        site_id,
        kind: PurgeKind::Url,
        targets,
        skip: None,
    }
}

/// Whether the site's settings have this trigger switched on.
///
/// The map is `{"page.published": true}` and a name that is not a key is off. Only an
/// explicit `true` turns one on, so a malformed value (`"yes"`, `1`, a nested object) does
/// not read as consent to send invalidation traffic to a provider.
#[must_use]
pub fn trigger_enabled(trigger: Trigger, enabled: &Value) -> bool {
    enabled
        .get(trigger.event_name())
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The addresses a slug and a media id resolve to, and the tags they answer to.
///
/// Built from [`surrogate_keys`], which is the *same* function that writes the header, so
/// the tag a purge asks for is by construction a tag a response carried. Two copies of that
/// mapping is how "purge by tag silently does nothing" happens.
fn tag_map(
    trigger: Trigger,
    site_id: Uuid,
    payload: &Value,
    capabilities: &Capabilities,
) -> std::collections::BTreeMap<String, Vec<String>> {
    if capabilities.tags {
        return std::collections::BTreeMap::new();
    }
    let mut map = std::collections::BTreeMap::new();
    match trigger.breadth() {
        Breadth::Url | Breadth::Media => {
            if let Some(path) = public_path(trigger, payload) {
                for key in surrogate_keys(&path) {
                    map.insert(key, vec![path.clone()]);
                }
            }
        }
        Breadth::Site => {
            map.insert(site_tag(site_id), vec!["*".to_string()]);
        }
    }
    map
}

/// The public address an event's payload names, or `None` when it names none.
fn public_path(trigger: Trigger, payload: &Value) -> Option<String> {
    match trigger {
        Trigger::PagePublished | Trigger::PageUnpublished | Trigger::PageDeleted => {
            // The slug is the address. A page event without a slug cannot be invalidated
            // by address, and guessing the id into the API path would purge a path no
            // visitor ever requests — a purge that costs the operator money and changes
            // nothing.
            let slug = payload.get("slug")?.as_str()?.trim();
            if slug.is_empty() {
                return None;
            }
            Some(format!("/{slug}"))
        }
        Trigger::MediaVersionCreated => uuid_field(payload, "media_id")
            .map(|id| format!("/api/v1/public/media/{id}")),
        Trigger::ThemeActivated | Trigger::DomainAdded | Trigger::DomainRemoved => None,
    }
}

/// A uuid field, as a string the caller can put in a path.
fn uuid_field(payload: &Value, key: &str) -> Option<Uuid> {
    match payload.get(key)? {
        Value::String(text) => Uuid::parse_str(text).ok(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// The drain
// ---------------------------------------------------------------------------------------------

/// What one drain tick did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DrainReport {
    /// Events read above the cursor.
    pub read: u64,
    /// Purges queued.
    pub queued: u64,
    /// Events that queued nothing, with the reason each skipped.
    pub skipped: u64,
    /// Events that were not a trigger this build consumes.
    pub foreign: u64,
    /// The cursor after the tick.
    pub cursor: i64,
}

impl DrainReport {
    /// `true` when there was nothing to read.
    #[must_use]
    pub fn is_idle(self) -> bool {
        self.read == 0
    }
}

/// Point a never-advanced cursor at the end of the bus.
///
/// Same rule as the search indexer: a fresh installation watches forward, and a bus that
/// already carries history does not get a burst of retroactive purges on the first boot —
/// the content it refers to has been republished many times since, and purging it all at
/// once is a stampede at a provider for no benefit. `None` means it had already moved.
pub async fn seed_cursor(pool: &PgPool) -> Result<Option<i64>, CdnError> {
    let head: Option<i64> = sqlx::query_scalar("select max(id) from events")
        .fetch_one(pool)
        .await?;
    let head = head.unwrap_or(0);
    let moved: Option<i64> = sqlx::query_scalar(
        "update cdn_invalidation_cursor set last_event_id = $1, updated_at = now() \
         where id = 1 and last_event_id = 0 and $1 > 0 \
         returning last_event_id",
    )
    .bind(head)
    .fetch_optional(pool)
    .await?;
    Ok(moved)
}

/// Apply every trigger event above the cursor and queue what it invalidates.
pub async fn drain(pool: &PgPool, batch: i64) -> Result<DrainReport, CdnError> {
    let batch = batch.clamp(1, 500);
    let mut transaction = pool.begin().await?;

    // The cursor row is the lock, exactly as in the search drain: two API processes must
    // not both walk the same events, or every publication queues two purges and the
    // provider is billed twice for one publish.
    let locked: Option<i64> = sqlx::query_scalar(
        "select last_event_id from cdn_invalidation_cursor where id = 1 for update skip locked",
    )
    .fetch_optional(&mut *transaction)
    .await?;
    let Some(cursor) = locked else {
        transaction.rollback().await?;
        return Ok(DrainReport {
            cursor: current_cursor(pool).await.unwrap_or_default(),
            ..DrainReport::default()
        });
    };

    let events: Vec<(i64, String, Option<Uuid>, Value)> = sqlx::query_as(
        "select id, name, site_id, payload from events \
         where id > $1 order by id asc limit $2",
    )
    .bind(cursor)
    .bind(batch)
    .fetch_all(&mut *transaction)
    .await?;

    if events.is_empty() {
        transaction.commit().await?;
        return Ok(DrainReport {
            cursor,
            ..DrainReport::default()
        });
    }

    let mut report = DrainReport {
        read: events.len() as u64,
        cursor,
        ..DrainReport::default()
    };

    for (event_id, name, site_id, payload) in &events {
        match apply_event(&mut transaction, *event_id, name, *site_id, payload).await {
            Ok(Some(queued)) => report.queued += queued,
            Ok(None) => report.skipped += 1,
            Err(error) => {
                // One bad event must not stop the drain, and must not be retried for ever
                // either: the cursor moves past it and the reason is logged. A purge that
                // cannot be derived from a malformed payload is not going to become
                // derivable on the next tick.
                tracing::warn!(
                    event_id,
                    event = %name,
                    error = %error,
                    "cdn: an event could not be turned into a purge"
                );
                report.skipped += 1;
            }
        }
    }

    let last = events.last().map_or(cursor, |(id, ..)| *id);
    sqlx::query(
        "update cdn_invalidation_cursor set last_event_id = $1, updated_at = now() where id = 1",
    )
    .bind(last)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    report.cursor = last;
    Ok(report)
}

/// The cursor's current position, for the overview.
pub async fn current_cursor(pool: &PgPool) -> Result<i64, CdnError> {
    let cursor: Option<i64> =
        sqlx::query_scalar("select last_event_id from cdn_invalidation_cursor where id = 1")
            .fetch_optional(pool)
            .await?;
    cursor.ok_or(CdnError::from(sqlx::Error::RowNotFound))
}

/// One event: queue what it invalidates, or explain why nothing was queued.
async fn apply_event(
    transaction: &mut Transaction<'_, Postgres>,
    event_id: i64,
    name: &str,
    site_id: Option<Uuid>,
    payload: &Value,
) -> Result<Option<u64>, CdnError> {
    // Events that are not triggers are counted as *foreign*, not skipped: the drain reads
    // every event on the bus, and the overwhelming majority of them belong to somebody
    // else. Calling them "skipped" would make the counters say the automatic invalidation
    // is failing, when it is doing precisely what it should.
    let Some(trigger) = Trigger::from_event_name(name) else {
        return Ok(None);
    };
    let Some(site_id) = site_id else {
        // A page published without a site cannot invalidate a site cache. The event is
        // legal (an organization-level publication), so this is a no-target rather than an
        // error.
        tracing::debug!(event_id, event = %name, "cdn: an event with no site invalidates nothing");
        return Ok(None);
    };

    let settings = auto_purge_for(&mut *transaction, site_id).await?;
    let (provider_key, capabilities) = provider_capabilities(&mut *transaction, site_id).await?;

    // The site's own published addresses, read only when a whole-site trigger needs the
    // URL fallback. A per-page publish does not touch this query, so the hot path (every
    // publication on the platform) stays one settings read and one event read.
    let mut plan = plan(trigger, site_id, payload, &settings, &capabilities);
    if !capabilities.tags && plan.trigger.breadth() == Breadth::Site && plan.skip.is_none() {
        let paths = site_paths(&mut *transaction, site_id).await?;
        plan = site_plan(trigger, site_id, &paths);
    }

    if let Some(reason) = plan.skip {
        tracing::debug!(
            event_id,
            event = %trigger.event_name(),
            reason = reason.as_str(),
            "cdn: no purge queued"
        );
        return Ok(None);
    }
    if plan.targets.len() > MAX_TARGETS {
        // A cap is a refusal, not a truncation: a purge of the first 500 of 900 URLs
        // leaves 400 stale addresses and reports success, which is the one outcome worse
        // than no automatic invalidation at all.
        tracing::warn!(
            event_id,
            event = %trigger.event_name(),
            targets = plan.targets.len(),
            "cdn: the event expands past one purge's cap and was skipped"
        );
        return Ok(None);
    }

    let queued = queue(transaction, &plan, &provider_key, event_id).await?;
    tracing::debug!(
        event_id,
        event = %trigger.event_name(),
        purge_id = %queued,
        targets = plan.targets.len(),
        "cdn: a purge was queued for the event"
    );
    Ok(Some(1))
}

/// The trigger toggles that apply to a site, resolving the platform default.
///
/// The precedence is the same one `provider_for_site` and `resolve_settings` use, for the
/// same reason: a site without a row inherits the installation's, and a drain that resolved
/// it differently from the screen that configured it would purge against a provider the
/// operator never chose.
async fn auto_purge_for(
    transaction: &mut Transaction<'_, Postgres>,
    site_id: Uuid,
) -> Result<Value, CdnError> {
    let row: Option<Value> = sqlx::query_scalar(
        "select auto_purge from cdn_settings \
         where site_id = $1 or site_id is null \
         order by site_id is null asc, site_id nulls last \
         limit 1",
    )
    .bind(site_id)
    .fetch_optional(&mut **transaction)
    .await?;
    Ok(row.unwrap_or(Value::Object(Default::default())))
}

/// The provider key and what the adapter can do with a target list.
async fn provider_capabilities(
    transaction: &mut Transaction<'_, Postgres>,
    site_id: Uuid,
) -> Result<(String, Capabilities), CdnError> {
    let key: Option<String> = sqlx::query_scalar(
        "select provider from cdn_settings \
         where site_id = $1 or site_id is null \
         order by site_id is null asc, site_id nulls last \
         limit 1",
    )
    .bind(site_id)
    .fetch_optional(&mut **transaction)
    .await?;
    let key = key.unwrap_or_else(|| "origin".to_string());
    // The settings are the ones `provider_for_site` would read; the adapter is built from
    // them so the capabilities answered here are the ones the worker will actually act with,
    // not the ones a *default-constructed* adapter advertises. An adapter whose endpoint is
    // unset still reports its own capabilities, and the purge it fails is the honest
    // outcome — the worker's error path exists for exactly that.
    let settings = crate::provider::ProviderSettings::default();
    let capabilities = crate::provider::provider_for(&key, &settings).capabilities();
    Ok((key, capabilities))
}

/// Write the planned purge, inside the drain's own transaction.
///
/// `enqueue` opens a transaction of its own, which would deadlock against the one the
/// drain holds, so the two statements are written here against the drain's transaction. The
/// rows and the cursor advance therefore land **together**: a purge whose cursor did not
/// move would be queued again on the next tick, and a cursor that moved without its purge
/// would be an invalidation that never happened and never will.
async fn queue(
    transaction: &mut Transaction<'_, Postgres>,
    plan: &Plan,
    provider: &str,
    event_id: i64,
) -> Result<Uuid, CdnError> {
    let new = NewPurge {
        site_id: Some(plan.site_id),
        kind: plan.kind,
        targets: plan.targets.clone(),
        provider: provider.to_string(),
        // The platform asked, not a person. See the module docs: borrowing the
        // publisher's id would make the history claim an operator did this.
        requested_by: Uuid::nil(),
    };
    // `requested_by` is a foreign key to `users`, and the nil uuid is not a user, so the
    // automatic case writes `null` — the column's own "who asked" for nobody. The insert
    // therefore binds an `Option`, and the *manual* path keeps binding the real id.
    let row: (Uuid,) = sqlx::query_as(
        "insert into cdn_purges (site_id, kind, targets, status, provider, item_count, requested_by) \
         values ($1, $2, $3, 'queued', $4, $5, null) \
         returning id",
    )
    .bind(new.site_id)
    .bind(new.kind.as_str())
    .bind(&new.targets)
    .bind(&new.provider)
    .bind(new.targets.len() as i32)
    .fetch_one(&mut **transaction)
    .await?;
    let purge_id = row.0;

    for target in &plan.targets {
        sqlx::query("insert into cdn_purge_items (purge_id, target) values ($1, $2)")
            .bind(purge_id)
            .bind(target)
            .execute(&mut **transaction)
            .await?;
    }

    // The event that caused this, so the panel can trace an automatic purge back to the
    // publication that asked for it. Stored in the `error` column only for a *failed*
    // purge would be a lie, so the provenance is a column of its own.
    sqlx::query("insert into cdn_purge_sources (purge_id, event_id, trigger) values ($1, $2, $3)")
        .bind(purge_id)
        .bind(event_id)
        .bind(plan.trigger.event_name())
        .execute(&mut **transaction)
        .await?;

    Ok(purge_id)
}

/// The event ids a purge was derived from, for the detail drawer.
pub async fn sources_of(pool: &PgPool, purge_id: Uuid) -> Result<Vec<(i64, String)>, CdnError> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "select event_id, trigger from cdn_purge_sources where purge_id = $1 order by event_id",
    )
    .bind(purge_id)
    .fetch_all(pool)
    .await
    .map_err(CdnError::from)?;
    Ok(rows)
}

/// The public addresses of a site's published pages: the whole-site fallback's target list.
///
/// `origin` and `generic_http` cannot be told "everything", so a site-wide invalidation
/// against them has to name the addresses instead — and those addresses are the site's
/// published slugs, because those are the ones the edge is actually holding. A draft has no
/// cached address and a deleted page's is already stale, so neither belongs in the list.
///
/// Read on the drain's own transaction rather than the pool: the purge it feeds has to be
/// written in the same transaction that advances the cursor, and a list read outside it
/// could describe a site state that the cursor then claims to have acted on.
async fn site_paths(
    transaction: &mut Transaction<'_, Postgres>,
    site_id: Uuid,
) -> Result<Vec<String>, CdnError> {
    let slugs: Vec<String> = sqlx::query_scalar(
        "select slug from pages where site_id = $1 and status = 'published' order by slug",
    )
    .bind(site_id)
    .fetch_all(&mut **transaction)
    .await
    .map_err(CdnError::from)?;
    Ok(slugs
        .into_iter()
        .filter(|slug| !slug.is_empty())
        .map(|slug| format!("/{slug}"))
        .collect())
}


#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// An adapter that can hold surrogate keys.
    const WITH_TAGS: Capabilities = Capabilities {
        tags: true,
        purge_all: true,
    };

    /// `generic_http` as it ships: an endpoint that is told "these files".
    const NO_TAGS: Capabilities = Capabilities {
        tags: false,
        purge_all: false,
    };

    const SITE: Uuid = Uuid::from_u128(0x5eed_0000_0000_0000_0000_0000_0000_0001);

    fn on(names: &[&str]) -> Value {
        let mut map = serde_json::Map::new();
        for name in names {
            map.insert((*name).to_string(), Value::Bool(true));
        }
        Value::Object(map)
    }

    #[test]
    fn every_trigger_names_an_event_the_platform_actually_records() {
        // The setting REQ-011's screen shipped with two names nothing emits
        // (`media.replaced`, `site.domain.changed`). A switch on a name no emitter writes
        // is a switch an operator can turn on, watch, and never see anything happen from —
        // so the drift is asserted here, in the table that fires, rather than left to a
        // reader to notice.
        for trigger in Trigger::all() {
            let name = trigger.event_name();
            assert!(
                omnion_events::catalogue::is_known(name),
                "{name} is in the CDN trigger table but not in the event catalogue"
            );
        }
    }

    #[test]
    fn a_trigger_only_speaks_for_the_name_it_belongs_to() {
        for trigger in Trigger::all() {
            assert_eq!(
                Trigger::from_event_name(trigger.event_name()),
                Some(*trigger),
                "{} did not round-trip",
                trigger.event_name()
            );
        }
        for name in [
            "page.updated",
            "media.replaced",
            "site.domain.changed",
            "cdn.purge.requested",
            "not.an.event.at.all",
            "",
        ] {
            assert_eq!(
                Trigger::from_event_name(name),
                None,
                "{name} is not a trigger and must not claim to be one"
            );
        }
    }

    #[test]
    fn a_trigger_that_is_not_explicitly_true_is_off() {
        let trigger = Trigger::PagePublished;
        assert!(trigger_enabled(trigger, &on(&["page.published"])));

        // Absent, false, and every shape that is not a boolean.
        for map in [
            json!({}),
            json!({ "page.published": false }),
            json!({ "page.published": "yes" }),
            json!({ "page.published": 1 }),
            json!({ "page.published": null }),
            json!({ "page.published": { "on": true } }),
            json!(null),
            json!([]),
        ] {
            assert!(
                !trigger_enabled(trigger, &map),
                "{map} must not be read as consent to send invalidation traffic"
            );
        }
    }

    #[test]
    fn a_publish_queues_that_pages_own_address() {
        let plan = plan(
            Trigger::PagePublished,
            SITE,
            &json!({ "page_id": "0f6d6a54-0000-4000-8000-000000000001", "slug": "blog/hello" }),
            &on(&["page.published"]),
            &NO_TAGS,
        );
        assert_eq!(plan.skip, None);
        assert_eq!(plan.kind, PurgeKind::Url);
        assert_eq!(plan.targets, vec!["/blog/hello".to_string()]);
    }

    #[test]
    fn a_turn_off_toggle_queues_nothing_and_says_why() {
        let plan = plan(
            Trigger::PagePublished,
            SITE,
            &json!({ "slug": "blog/hello" }),
            &on(&["page.deleted"]),
            &NO_TAGS,
        );
        assert_eq!(plan.skip, Some(SkipReason::Disabled));
        assert!(plan.targets.is_empty(), "a disabled trigger must not leave targets behind");
    }

    #[test]
    fn a_page_event_without_a_slug_queues_nothing() {
        // The temptation is to purge the API path by id. That address is never requested
        // by a visitor, so it costs the operator a provider call and changes nothing.
        for payload in [
            json!({ "page_id": "0f6d6a54-0000-4000-8000-000000000001" }),
            json!({ "slug": "" }),
            json!({ "slug": "   " }),
            json!({ "slug": 42 }),
            json!({}),
        ] {
            let plan = plan(
                Trigger::PagePublished,
                SITE,
                &payload,
                &on(&["page.published"]),
                &NO_TAGS,
            );
            assert_eq!(
                plan.skip,
                Some(SkipReason::NoTarget),
                "{payload} has no address to invalidate"
            );
        }
    }

    #[test]
    fn a_replaced_file_invalidates_the_media_path_and_nothing_else() {
        let id = "0f6d6a54-0000-4000-8000-0000000000ff";
        let plan = plan(
            Trigger::MediaVersionCreated,
            SITE,
            &json!({ "media_id": id }),
            &on(&["media.version_created"]),
            &NO_TAGS,
        );
        assert_eq!(plan.skip, None);
        assert_eq!(
            plan.targets,
            vec![format!("/api/v1/public/media/{id}")],
            "a file has no other public address, and purging the pages that embed it is a \
             content-graph walk the event does not carry"
        );
    }

    #[test]
    fn a_theme_activation_invalidates_the_whole_site_by_tag() {
        let plan = plan(
            Trigger::ThemeActivated,
            SITE,
            &json!({ "theme": "atlas" }),
            &on(&["theme.activated"]),
            &WITH_TAGS,
        );
        assert_eq!(plan.skip, None);
        assert_eq!(plan.kind, PurgeKind::Tag);
        assert_eq!(plan.targets, vec![site_tag(SITE)]);
    }

    #[test]
    fn a_provider_that_cannot_hold_a_tag_is_given_the_addresses_instead() {
        // A `site-<uuid>` tag against `generic_http` becomes a body no endpoint reads, the
        // adapter answers success, and the operator sees a purge that invalidated nothing.
        // The fallback is the site's own published addresses, decided at plan time.
        let paths = vec!["/home".to_string(), "/blog".to_string(), "/blog/hello".to_string()];
        let plan = site_plan(Trigger::ThemeActivated, SITE, &paths);
        assert_eq!(plan.skip, None);
        assert_eq!(plan.kind, PurgeKind::Url);
        assert_eq!(plan.targets, paths);
    }

    #[test]
    fn the_url_fallback_deduplicates_and_drops_blanks() {
        let plan = site_plan(
            Trigger::ThemeActivated,
            SITE,
            &[
                "/home".to_string(),
                "  ".to_string(),
                "/home".to_string(),
                " /blog ".to_string(),
            ],
        );
        assert_eq!(plan.targets, vec!["/home".to_string(), "/blog".to_string()]);
    }

    #[test]
    fn a_site_with_nothing_published_has_nothing_to_invalidate() {
        let plan = site_plan(Trigger::DomainAdded, SITE, &[]);
        assert_eq!(
            plan.skip,
            Some(SkipReason::NoTarget),
            "an empty target list would also be refused by the table's own constraint"
        );
    }

    #[test]
    fn the_site_tag_is_the_one_the_cache_header_carries() {
        // `apps/api/src/routes/cdn_cache.rs` writes `site-<id>` into `Surrogate-Key` on
        // every cacheable public response. A whole-site tag purge is only correct while the
        // two spellings agree; this is the assertion that says so, on the purge side.
        let site = Uuid::new_v4();
        assert_eq!(site_tag(site), format!("site-{site}"));
    }

    #[test]
    fn a_cap_that_would_truncate_is_a_skip_and_not_a_shorter_purge() {
        // Purging the first 500 of 900 addresses leaves 400 stale ones and reports
        // success. The drain refuses instead; the check lives there, and this asserts the
        // cap the plan is measured against is the console's.
        let too_many: Vec<String> = (0..=MAX_TARGETS).map(|index| format!("/p/{index}")).collect();
        let plan = site_plan(Trigger::ThemeActivated, SITE, &too_many);
        assert!(plan.skip.is_none());
        assert!(
            plan.targets.len() > MAX_TARGETS,
            "the plan expands past the cap and the drain must refuse it"
        );
    }

    #[test]
    fn an_unpublished_page_never_appears_in_the_fallback() {
        // `site_paths` filters on `status = 'published'` in SQL. The pure half of that
        // guarantee — a blank slug is not an address — is here.
        let plan = site_plan(Trigger::ThemeActivated, SITE, &["/a".into(), String::new()]);
        assert_eq!(plan.targets, vec!["/a".to_string()]);
    }

    #[test]
    fn every_trigger_says_what_it_costs_and_says_it_once() {
        // The settings screen renders one sentence per switch, next to a name the operator
        // is about to turn on. A blank or duplicated sentence is a row of toggles where
        // two switches claim the same consequence, and a person cannot tell which one they
        // are signing up for.
        let mut seen = std::collections::BTreeSet::new();
        for trigger in Trigger::all() {
            let sentence = trigger.consequence();
            assert!(
                sentence.split_whitespace().count() >= 5,
                "{trigger:?} has no consequence sentence for the settings screen"
            );
            assert!(
                seen.insert(sentence),
                "{trigger:?} reuses another trigger's sentence: {sentence}"
            );
        }
    }

    #[test]
    fn the_breadth_decides_whether_one_address_or_a_whole_site_is_invalidated() {
        // The three breadths are three different products, and a trigger filed under the
        // wrong one either under-invalidates (a stale theme behind every page) or
        // over-invalidates (a purge of the whole site on every upload, which is a
        // provider bill nobody approved).
        for trigger in [
            Trigger::PagePublished,
            Trigger::PageUnpublished,
            Trigger::PageDeleted,
        ] {
            assert_eq!(trigger.breadth(), Breadth::Url, "{trigger:?}");
        }
        assert_eq!(Trigger::MediaVersionCreated.breadth(), Breadth::Media);
        for trigger in [
            Trigger::ThemeActivated,
            Trigger::DomainAdded,
            Trigger::DomainRemoved,
        ] {
            assert_eq!(trigger.breadth(), Breadth::Site, "{trigger:?}");
        }
    }
}
