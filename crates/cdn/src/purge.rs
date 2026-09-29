//! The purge queue and its history (docs/requests/REQ-011, slice 2).
//!
//! Slice 1 put the *policy* in this crate: whether a response may be cached, for how long
//! and under which key. This module holds the other half — the durable record of asking a
//! provider to forget something, and the states that record passes through.
//!
//! It is here rather than in `apps/api` for the same reason the matcher is: the interesting
//! parts (what counts as a valid target list, what an outcome means for the parent row, how
//! the backoff grows) are decisions with no I/O in them, and a decision that can only be
//! exercised through an HTTP call and a live provider is a decision nobody will test.
//!
//! The adapter calls themselves are blocking by design (see [`crate::provider`]) and happen
//! in the worker, never here: this module only decides *what* should be attempted and
//! *records* what came back.

use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::CdnError;
use crate::provider::{MAX_BATCH, Purge, PurgeOutcome};

/// The cap on targets one console submission may carry.
///
/// The request specifies 500. The provider's own batch cap is the same number, so a
/// submission at the cap is exactly one provider call and anything above it is a
/// submission that the worker will split — which is legal, and the point is that the
/// console *refuses* it rather than silently accepting a paste of 900 lines and turning
/// it into a two-call operation nobody was told about.
pub const MAX_TARGETS: usize = 500;

/// The kind of invalidation, as stored in `cdn_purges.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurgeKind {
    /// Specific absolute paths.
    Url,
    /// Surrogate keys.
    Tag,
    /// The provider's whole zone.
    All,
}

impl PurgeKind {
    /// Parse the wire form.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "url" => Some(PurgeKind::Url),
            "tag" => Some(PurgeKind::Tag),
            "all" => Some(PurgeKind::All),
            _ => None,
        }
    }

    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            PurgeKind::Url => "url",
            PurgeKind::Tag => "tag",
            PurgeKind::All => "all",
        }
    }

    /// The provider request this kind produces.
    #[must_use]
    pub fn to_purge(self, targets: &[String]) -> Purge {
        match self {
            PurgeKind::All => Purge::All,
            PurgeKind::Url => Purge::Urls {
                targets: targets.to_vec(),
            },
            PurgeKind::Tag => Purge::Tags {
                targets: targets.to_vec(),
            },
        }
    }
}

/// Where a purge is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurgeStatus {
    /// Written, not yet claimed by a worker.
    Queued,
    /// Claimed; items are in flight.
    Running,
    /// Every item reached the provider successfully.
    Succeeded,
    /// Some items went through and some did not.
    Partial,
    /// Nothing went through, or every retry was exhausted.
    Failed,
}

impl PurgeStatus {
    /// Parse the stored spelling.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "queued" => Some(PurgeStatus::Queued),
            "running" => Some(PurgeStatus::Running),
            "succeeded" => Some(PurgeStatus::Succeeded),
            "partial" => Some(PurgeStatus::Partial),
            "failed" => Some(PurgeStatus::Failed),
            _ => None,
        }
    }

    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            PurgeStatus::Queued => "queued",
            PurgeStatus::Running => "running",
            PurgeStatus::Succeeded => "succeeded",
            PurgeStatus::Partial => "partial",
            PurgeStatus::Failed => "failed",
        }
    }

    /// Whether the drawer offers a retry for this state.
    ///
    /// Only the two states that carry a failure do. A `succeeded` purge has nothing to
    /// retry and a `running` one has a worker that is already about to retry, so a button
    /// on either of them is a button that does nothing.
    #[must_use]
    pub fn retryable(self) -> bool {
        matches!(self, PurgeStatus::Partial | PurgeStatus::Failed)
    }
}

/// Where a single target is in its own life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemStatus {
    /// Not yet attempted, or awaiting its next attempt.
    Pending,
    /// Claimed by a worker.
    Running,
    /// The provider accepted it.
    Done,
    /// Every attempt was refused, or the attempt budget ran out.
    Failed,
}

impl ItemStatus {
    /// Parse the stored spelling.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(ItemStatus::Pending),
            "running" => Some(ItemStatus::Running),
            "done" => Some(ItemStatus::Done),
            "failed" => Some(ItemStatus::Failed),
            _ => None,
        }
    }

    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ItemStatus::Pending => "pending",
            ItemStatus::Running => "running",
            ItemStatus::Done => "done",
            ItemStatus::Failed => "failed",
        }
    }
}

/// What the console submitted, already parsed.
#[derive(Debug, Clone)]
pub struct NewPurge {
    /// The site whose cache is being invalidated.
    pub site_id: Option<Uuid>,
    /// Which kind of invalidation.
    pub kind: PurgeKind,
    /// The validated targets, already trimmed and de-duplicated.
    pub targets: Vec<String>,
    /// The adapter that will run it, captured at request time.
    pub provider: String,
    /// Who asked.
    pub requested_by: Uuid,
}

/// A stored purge row.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PurgeRow {
    /// Primary key.
    pub id: Uuid,
    /// The site, or `null` once the site is gone.
    pub site_id: Option<Uuid>,
    /// Stored kind.
    pub kind: String,
    /// The targets as submitted.
    pub targets: Vec<String>,
    /// Stored status.
    pub status: String,
    /// The adapter that ran it.
    pub provider: String,
    /// How many items it expanded into.
    pub item_count: i32,
    /// How many of those failed.
    pub failed_count: i32,
    /// Who asked, or `null` if that account is gone.
    pub requested_by: Option<Uuid>,
    /// When it was asked for.
    pub requested_at: OffsetDateTime,
    /// When a worker first claimed it.
    pub started_at: Option<OffsetDateTime>,
    /// When it reached a terminal state.
    pub finished_at: Option<OffsetDateTime>,
    /// The provider's message, verbatim.
    pub error: Option<String>,
}

/// One target inside a purge.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PurgeItemRow {
    /// Primary key.
    pub id: i64,
    /// The purge this belongs to.
    pub purge_id: Uuid,
    /// The absolute path or tag.
    pub target: String,
    /// Stored status.
    pub status: String,
    /// How many times it has been attempted.
    pub attempts: i32,
    /// When it may next be attempted.
    pub next_attempt_at: OffsetDateTime,
    /// The provider's HTTP status, when there was one.
    pub response_status: Option<i32>,
    /// The provider's message, verbatim.
    pub error: Option<String>,
    /// When it reached a terminal state.
    pub done_at: Option<OffsetDateTime>,
}

/// The filters `GET /api/v1/cdn/purges` accepts.
#[derive(Debug, Clone, Default)]
pub struct PurgeFilter {
    /// Restrict to one site.
    pub site_id: Option<Uuid>,
    /// Restrict to one status.
    pub status: Option<PurgeStatus>,
    /// Restrict to one kind.
    pub kind: Option<PurgeKind>,
    /// Only rows at or after this instant.
    pub since: Option<OffsetDateTime>,
    /// Only rows at or before this instant.
    pub until: Option<OffsetDateTime>,
    /// Page size.
    pub limit: i64,
    /// Rows to skip.
    pub offset: i64,
}

impl PurgeFilter {
    /// The page size used when the caller sends none.
    pub const DEFAULT_LIMIT: i64 = 50;
    /// The largest page a caller may ask for.
    pub const MAX_LIMIT: i64 = 200;
}

/// A page of history plus the total, so the panel can say "showing 50 of 312".
#[derive(Debug, Default)]
pub struct PurgePage {
    /// The rows of this page.
    pub purges: Vec<PurgeRow>,
    /// How many rows match the filter in total.
    pub total: i64,
}

// ---------------------------------------------------------------------------------------------
// Validation — the console's rules, in one place
// ---------------------------------------------------------------------------------------------

/// What the console submission was refused for.
///
/// Each variant maps to one form field, because "invalid input" with no field is a message
/// the operator has to guess where to look.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PurgeInputError {
    /// The kind string was not one of the three.
    #[error("kind must be one of url, tag or all")]
    Kind,
    /// A URL or tag purge was submitted with an empty target list.
    #[error("a {kind} purge needs at least one target")]
    EmptyTargets {
        /// Which kind was submitted without targets.
        kind: &'static str,
    },
    /// A whole-zone purge was submitted without the typed confirmation.
    #[error("purging the whole zone needs the word PURGE typed in the confirmation box")]
    AllNeedsConfirmation,
    /// More targets than the console accepts.
    #[error("a purge takes at most {MAX_TARGETS} targets; {given} were given")]
    TooManyTargets {
        /// How many were given.
        given: usize,
    },
    /// A URL target that is not an absolute path.
    #[error("a URL target must start with / and cannot contain whitespace")]
    MalformedUrl,
    /// A tag target outside the surrogate-key format.
    #[error("a tag may only contain letters, digits, '-', '_' and '.'")]
    MalformedTag,
}

impl PurgeInputError {
    /// The form field this belongs to.
    #[must_use]
    pub fn field(&self) -> &'static str {
        match self {
            PurgeInputError::Kind
            | PurgeInputError::EmptyTargets { .. }
            | PurgeInputError::AllNeedsConfirmation
            | PurgeInputError::TooManyTargets { .. } => "targets",
            PurgeInputError::MalformedUrl => "targets",
            PurgeInputError::MalformedTag => "targets",
        }
    }
}

/// Validate what the console submitted and normalise the targets.
///
/// Normalisation is three steps in this order, and the order matters: lines are trimmed and
/// blanks dropped (so a paste with a trailing newline is not one target too many), duplicates
/// are dropped (**a page published twice must not invalidate twice** — the provider bills
/// and rate-limits per call, and a duplicate target is the commonest way a purge storm
/// starts), and only then is the count checked against the cap. Checking the cap first
/// would refuse a paste of 500 targets that contained 40 blank lines, which is a submission
/// the user made correctly.
///
/// The typed `PURGE` confirmation is checked here rather than in the route, because "this
/// box says PURGE" is a property of the *request*, and a second caller that forgets the
/// confirmation should be refused by the same code rather than by a different one.
pub fn validate(
    kind: PurgeKind,
    raw_targets: &[String],
    zone_confirmed: bool,
) -> Result<Vec<String>, PurgeInputError> {
    if kind == PurgeKind::All {
        if !zone_confirmed {
            return Err(PurgeInputError::AllNeedsConfirmation);
        }
        // An `all` purge still records a target list, and the single entry makes the
        // history row legible ("what did this purge?") without pretending it was selective.
        return Ok(vec!["*".to_string()]);
    }

    let mut targets: Vec<String> = Vec::new();
    for line in raw_targets {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match kind {
            PurgeKind::Url => {
                if !trimmed.starts_with('/')
                    || trimmed.starts_with("//")
                    || trimmed.chars().any(char::is_whitespace)
                {
                    return Err(PurgeInputError::MalformedUrl);
                }
            }
            PurgeKind::Tag => {
                // The surrogate-key format `headers::surrogate_keys` emits. Accepting a
                // looser string here would let an operator queue a tag the provider will
                // reject at drain time, which is a failure an hour later instead of now.
                if !trimmed
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/'))
                    || trimmed.is_empty()
                {
                    return Err(PurgeInputError::MalformedTag);
                }
            }
            PurgeKind::All => unreachable!("handled above"),
        }
        if !targets.iter().any(|existing| existing == trimmed) {
            targets.push(trimmed.to_string());
        }
    }

    if targets.is_empty() {
        return Err(PurgeInputError::EmptyTargets {
            kind: kind.as_str(),
        });
    }
    if targets.len() > MAX_TARGETS {
        return Err(PurgeInputError::TooManyTargets {
            given: targets.len(),
        });
    }
    Ok(targets)
}

/// Split a target list into the provider calls it needs.
///
/// The provider's own cap is [`MAX_BATCH`]; the console's is [`MAX_TARGETS`], so a
/// programmatic caller (an event subscriber, a future bulk tool) can still hand over a
/// thousand targets and have them split correctly rather than be refused.
#[must_use]
pub fn batches(kind: PurgeKind, targets: &[String]) -> Vec<Purge> {
    match kind {
        PurgeKind::All => vec![Purge::All],
        PurgeKind::Url => {
            chunks(&targets, MAX_BATCH)
                .into_iter()
                .map(|chunk| Purge::Urls {
                    targets: chunk.to_vec(),
                })
                .collect()
        }
        PurgeKind::Tag => {
            chunks(&targets, MAX_BATCH)
                .into_iter()
                .map(|chunk| Purge::Tags {
                    targets: chunk.to_vec(),
                })
                .collect()
        }
    }
}

/// Borrow a slice into chunks of at most `size`, never producing an empty chunk.
///
/// A `chunks(0)` panics and a `chunks` over an empty slice yields nothing at all, so the
/// caller that receives no batch must treat it as "nothing to do" rather than as an error —
/// which is why this returns an empty `Vec` instead of a `Result`.
fn chunks(targets: &[String], size: usize) -> Vec<&[String]> {
    if targets.is_empty() {
        return Vec::new();
    }
    targets.chunks(size.max(1)).collect()
}

/// How long to wait before attempt `attempts + 1`.
///
/// Exponential, capped at five minutes, and **jittered**. The jitter is the part that is
/// not optional: a page published with forty assets produces forty items that all fail
/// against the same provider at the same moment, and an un-jittered backoff puts them all
/// back in the same millisecond when the backoff expires. A provider that was briefly down
/// then receives the identical thundering herd that took it down, and the purge that was
/// supposed to heal the cache is the thing that keeps it down.
///
/// The jitter is derived from the item id rather than from a random number so a test can
/// assert the *bounds* (never negative, never beyond the cap) without pinning a value, and
/// so two workers claiming disjoint batches still get different offsets from the same
/// attempt number.
#[must_use]
pub fn backoff_seconds(attempts: i32, salt: i64) -> i64 {
    const BASE: i64 = 2;
    const CAP: i64 = 300;
    let exponent = attempts.clamp(0, 16) as u32;
    // `1 << exponent` would overflow a naive i64 past 63; clamping the exponent to 16
    // keeps the shift defined and the cap makes the value irrelevant beyond it anyway.
    let raw = BASE.saturating_mul(1_i64 << exponent.min(20));
    let bounded = raw.min(CAP);
    // A stable, cheap spread across the bucket: the item id mixed with the attempt count,
    // so the same item at the same attempt always gets the same offset (a worker restart
    // does not reshuffle a schedule it is already following) but two items failing
    // together do not.
    let spread = (salt.unsigned_abs() % (bounded as u64).max(1)) as i64;
    bounded - bounded / 2 + spread
}

/// Fold item outcomes into the parent purge's terminal state and message.
///
/// The parent is derived, never set directly by a caller: two places computing "is this
/// partial?" is how a `succeeded` row ends up over a failed item.
#[must_use]
pub fn summarise(items: &[ItemStatus]) -> (PurgeStatus, i32) {
    let failed = items.iter().filter(|s| **s == ItemStatus::Failed).count() as i32;
    let settled = items
        .iter()
        .filter(|s| **s == ItemStatus::Done || **s == ItemStatus::Failed)
        .count() as i32;
    if settled < items.len() as i32 {
        // Something is still in flight. The parent is not terminal yet, and saying
        // "succeeded" here would be the exact failure this queue exists to prevent.
        return (PurgeStatus::Running, failed);
    }
    let status = if failed == 0 {
        PurgeStatus::Succeeded
    } else if failed == items.len() as i32 {
        PurgeStatus::Failed
    } else {
        PurgeStatus::Partial
    };
    (status, failed)
}

/// The message a failed purge shows, from the provider's own words.
///
/// Prefers the first failing item's message over the parent's, because a `partial` purge's
/// parent message is the provider summarising and an item's is the provider being specific.
#[must_use]
pub fn failure_message(items: &[PurgeItemRow], fallback: Option<&str>) -> Option<String> {
    items
        .iter()
        .find(|item| item.error.as_deref().is_some_and(|error| !error.is_empty()))
        .map(|item| item.error.clone().expect("just found one"))
        .or_else(|| fallback.map(str::to_owned))
}

/// Record a provider outcome onto the items it covers.
///
/// `succeeded` is the set the provider confirmed; anything in `covered` that is not in it
/// failed. This is the shape every adapter returns — a set of confirmed targets plus a
/// message — and having one function apply it to the rows means the "which items failed"
/// question has exactly one answer in the codebase.
pub fn apply_outcome(
    items: &mut [PurgeItemRow],
    outcome: &PurgeOutcome,
    now: OffsetDateTime,
    max_attempts: i32,
    salt: i64,
) -> (i32, Option<String>) {
    let mut failed = 0;
    let mut message: Option<String> = None;

    for item in items.iter_mut() {
        match outcome {
            PurgeOutcome::Succeeded => {
                item.status = ItemStatus::Done.as_str().to_string();
                item.attempts += 1;
                item.error = None;
                item.response_status = None;
                item.done_at = Some(now);
                item.next_attempt_at = now;
            }
            PurgeOutcome::Partial { failed: refused, message: why } => {
                if refused.iter().any(|target| *target == item.target) {
                    fail_item(item, why, now, max_attempts, salt, &mut failed, &mut message);
                } else {
                    item.status = ItemStatus::Done.as_str().to_string();
                    item.attempts += 1;
                    item.error = None;
                    item.done_at = Some(now);
                    item.next_attempt_at = now;
                }
            }
            PurgeOutcome::Failed { message: why } => {
                fail_item(item, why, now, max_attempts, salt, &mut failed, &mut message);
            }
        }
    }
    (failed, message)
}

/// Mark one item failed, or leave it pending for another attempt.
fn fail_item(
    item: &mut PurgeItemRow,
    why: &str,
    now: OffsetDateTime,
    max_attempts: i32,
    salt: i64,
    failed: &mut i32,
    message: &mut Option<String>,
) {
    item.attempts += 1;
    if message.is_none() && !why.is_empty() {
        *message = Some(why.to_string());
    }
    if item.attempts >= max_attempts.max(1) {
        item.status = ItemStatus::Failed.as_str().to_string();
        item.error = Some(why.to_string());
        item.done_at = Some(now);
        *failed += 1;
    } else {
        // Still within the attempt budget: it goes back in the queue with a backoff, and
        // the purge stays `running`. Exhausting the budget is what makes it `failed`.
        item.status = ItemStatus::Pending.as_str().to_string();
        item.error = Some(why.to_string());
        item.done_at = None;
        item.next_attempt_at = now
            + time::Duration::seconds(backoff_seconds(item.attempts - 1, salt + item.id));
    }
}

// ---------------------------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------------------------

const PURGE_COLUMNS: &str = "id, site_id, kind, targets, status, provider, item_count, \
                           failed_count, requested_by, requested_at, started_at, finished_at, error";

const ITEM_COLUMNS: &str = "id, purge_id, target, status, attempts, next_attempt_at, \
                            response_status, error, done_at";

/// The same projection, qualified with the alias the claim's `update` uses.
///
/// `ITEM_COLUMNS` is for plain `select` statements, where the column names are the whole
/// projection. In an `update ... from` with a CTE both relations are in scope, so every
/// name must be qualified — and one unqualified name makes the whole statement fail, not
/// just that column.
const RETURNING_ITEMS: &str = "i.id, i.purge_id, i.target, i.status, i.attempts, \
                               i.next_attempt_at, i.response_status, i.error, i.done_at";

/// Write a purge and one item row per target, in one transaction.
///
/// The transaction is the whole point: a purge row with no items is a queue entry that will
/// never be claimed and never fail, so it sits in `queued` for ever and the operator
/// watching the queue depth sees a number that will not move and no explanation. The
/// insert and its items either both land or neither does.
pub async fn enqueue(pool: &sqlx::PgPool, purge: &NewPurge, targets: &[String]) -> Result<PurgeRow, CdnError> {
    let mut tx = pool.begin().await?;

    let row: PurgeRow = sqlx::query_as(&format!(
        "insert into cdn_purges (site_id, kind, targets, status, provider, item_count, requested_by) \
         values ($1, $2, $3, 'queued', $4, $5, $6) \
         returning {PURGE_COLUMNS}"
    ))
    .bind(purge.site_id)
    .bind(purge.kind.as_str())
    .bind(targets)
    .bind(&purge.provider)
    .bind(targets.len() as i32)
    .bind(purge.requested_by)
    .fetch_one(&mut *tx)
    .await?;

    for target in targets {
        sqlx::query(
            "insert into cdn_purge_items (purge_id, target) values ($1, $2)",
        )
        .bind(row.id)
        .bind(target)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(row)
}

/// One page of purge history, plus the unpaged total.
pub async fn list(pool: &sqlx::PgPool, filter: &PurgeFilter) -> Result<PurgePage, CdnError> {
    let limit = filter.limit.clamp(1, PurgeFilter::MAX_LIMIT);
    let offset = filter.offset.max(0);

    let total: i64 = sqlx::query_scalar(
        "select count(*) from cdn_purges \
         where ($1::uuid is null or site_id = $1) \
           and ($2::text is null or status = $2) \
           and ($3::text is null or kind = $3) \
           and ($4::timestamptz is null or requested_at >= $4) \
           and ($5::timestamptz is null or requested_at <= $5)",
    )
    .bind(filter.site_id)
    .bind(filter.status.map(PurgeStatus::as_str))
    .bind(filter.kind.map(PurgeKind::as_str))
    .bind(filter.since)
    .bind(filter.until)
    .fetch_one(pool)
    .await?;

    let purges = sqlx::query_as::<_, PurgeRow>(&format!(
        "select {PURGE_COLUMNS} from cdn_purges \
         where ($1::uuid is null or site_id = $1) \
           and ($2::text is null or status = $2) \
           and ($3::text is null or kind = $3) \
           and ($4::timestamptz is null or requested_at >= $4) \
           and ($5::timestamptz is null or requested_at <= $5) \
         order by requested_at desc, id desc \
         limit $6 offset $7"
    ))
    .bind(filter.site_id)
    .bind(filter.status.map(PurgeStatus::as_str))
    .bind(filter.kind.map(PurgeKind::as_str))
    .bind(filter.since)
    .bind(filter.until)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;

    Ok(PurgePage { purges, total })
}

/// One purge by id.
pub async fn find(pool: &sqlx::PgPool, id: Uuid) -> Result<Option<PurgeRow>, CdnError> {
    sqlx::query_as::<_, PurgeRow>(&format!(
        "select {PURGE_COLUMNS} from cdn_purges where id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(CdnError::from)
}

/// Every item of one purge, in listing order.
pub async fn items_of(pool: &sqlx::PgPool, purge_id: Uuid) -> Result<Vec<PurgeItemRow>, CdnError> {
    sqlx::query_as::<_, PurgeItemRow>(&format!(
        "select {ITEM_COLUMNS} from cdn_purge_items where purge_id = $1 order by id asc"
    ))
    .bind(purge_id)
    .fetch_all(pool)
    .await
    .map_err(CdnError::from)
}

/// Claim the next due batch of items, atomically.
///
/// Two things are deliberate here. The whole thing is **one statement** with
/// `for update skip locked`, so two API processes draining the same queue cannot both take
/// the same row — a read-then-update in Rust has a gap wide enough for exactly that. And the
/// claim filters on `status = 'pending' and next_attempt_at <= now()`, so an item that is
/// waiting out a backoff is invisible to the worker until its time comes, which is what
/// makes the backoff a schedule rather than a suggestion.
///
/// Rows are returned rather than just counted: the worker needs the targets to hand the
/// provider, and re-reading them after the claim would race the very update that just
/// happened.
pub async fn claim_due(
    pool: &sqlx::PgPool,
    limit: i64,
) -> Result<Vec<PurgeItemRow>, CdnError> {
    // The RETURNING list is qualified with the table alias, and that is not a style choice.
    // `returning id` is ambiguous here: the `due` CTE exposes an `id` column of its own, and
    // PostgreSQL resolves the bare name against both relations and refuses the statement with
    // `column reference "id" is ambiguous` — at runtime, on the first item the worker ever
    // claimed. Every other column in the list is qualified for the same reason; `purge_id`,
    // `target` and `status` would be ambiguous too if `due` ever grew them.
    sqlx::query_as::<_, PurgeItemRow>(&format!(
        "with due as ( \
            select i.id as claimed_id from cdn_purge_items i \
            where i.status = 'pending' and i.next_attempt_at <= now() \
            order by i.next_attempt_at asc, i.id asc \
            limit $1 \
            for update skip locked \
         ) \
         update cdn_purge_items i \
         set status = 'running' \
         from due \
         where i.id = due.claimed_id \
         returning {RETURNING_ITEMS}",
    ))
    .bind(limit.clamp(1, MAX_BATCH as i64))
    .fetch_all(pool)
    .await
    .map_err(CdnError::from)
}

/// Mark the parent purges of the given items as `running`, stamping the first start.
pub async fn mark_running(pool: &sqlx::PgPool, purge_ids: &[Uuid]) -> Result<(), CdnError> {
    if purge_ids.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "update cdn_purges \
         set status = 'running', started_at = coalesce(started_at, now()) \
         where id = any($1) and status = 'queued'",
    )
    .bind(purge_ids)
    .execute(pool)
    .await?;
    Ok(())
}

/// Persist the outcomes the worker computed.
pub async fn save_items(
    pool: &sqlx::PgPool,
    items: &[PurgeItemRow],
) -> Result<(), CdnError> {
    if items.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = items.iter().map(|item| item.id).collect();
    let statuses: Vec<String> = items.iter().map(|item| item.status.clone()).collect();
    let attempts: Vec<i32> = items.iter().map(|item| item.attempts).collect();
    let errors: Vec<Option<String>> = items.iter().map(|item| item.error.clone()).collect();
    let done_ats: Vec<Option<OffsetDateTime>> = items.iter().map(|item| item.done_at).collect();
    let nexts: Vec<OffsetDateTime> = items.iter().map(|item| item.next_attempt_at).collect();

    // `unnest` with ordinality keeps each array aligned by position, which is the only way
    // a per-row update can carry five different values. Five `case when id = $n` branches
    // would work too and would put a statement length limit on the batch size.
    sqlx::query(
        "update cdn_purge_items i \
         set status = v.status, \
             attempts = v.attempts, \
             error = v.error, \
             done_at = v.done_at, \
             next_attempt_at = v.next_attempt_at \
         from ( \
            select * from unnest($1::bigint[], $2::text[], $3::int[], $4::text[], \
                                 $5::timestamptz[], $6::timestamptz[]) \
                 as t(id, status, attempts, error, done_at, next_attempt_at) \
         ) v \
         where i.id = v.id",
    )
    .bind(&ids)
    .bind(&statuses)
    .bind(&attempts)
    .bind(&errors)
    .bind(&done_ats)
    .bind(&nexts)
    .execute(pool)
    .await?;
    Ok(())
}

/// Recompute and store each affected purge's terminal state.
///
/// Called after the items are written, and only for the purges those items belong to: a
/// sweep over the whole table would stamp `finished_at` on purges whose items are still
/// waiting out a backoff.
pub async fn settle(pool: &sqlx::PgPool, purge_ids: &[Uuid]) -> Result<(), CdnError> {
    for id in purge_ids {
        let items = items_of(pool, *id).await?;
        if items.is_empty() {
            continue;
        }
        let statuses: Vec<ItemStatus> = items
            .iter()
            .filter_map(|item| ItemStatus::parse(&item.status))
            .collect();
        let (status, failed) = summarise(&statuses);
        let error = if failed > 0 {
            failure_message(&items, None)
        } else {
            None
        };
        let finished = matches!(
            status,
            PurgeStatus::Succeeded | PurgeStatus::Partial | PurgeStatus::Failed
        );
        sqlx::query(
            "update cdn_purges \
             set status = $2, failed_count = $3, error = $4, \
                 finished_at = case when $5 then coalesce(finished_at, now()) else null end \
             where id = $1",
        )
        .bind(id)
        .bind(status.as_str())
        .bind(failed)
        .bind(&error)
        .bind(finished)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Requeue only the failed items of a purge, resetting their attempt budget.
///
/// "Only the failed" is the whole point of a retry. Re-running a `partial` purge end to end
/// would re-send the targets that already went through, which on a metered provider is both
/// slower and *rate-limit pressure against a provider that is already struggling* — the
/// failure mode where retrying makes the outage longer.
pub async fn requeue_failed(
    pool: &sqlx::PgPool,
    purge_id: Uuid,
) -> Result<u32, CdnError> {
    let result = sqlx::query(
        "update cdn_purge_items \
         set status = 'pending', attempts = 0, error = null, done_at = null, \
             next_attempt_at = now() \
         where purge_id = $1 and status = 'failed'",
    )
    .bind(purge_id)
    .execute(pool)
    .await?;

    // The parent goes back to `queued` so the queue-depth counter and the history filter
    // both see it again; its `finished_at` is cleared because it is not finished.
    sqlx::query(
        "update cdn_purges \
         set status = 'queued', failed_count = 0, error = null, finished_at = null \
         where id = $1 and status in ('failed', 'partial')",
    )
    .bind(purge_id)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() as u32)
}

/// How many items are waiting, and how many purges are still open.
///
/// The overview's "queue depth" is two numbers rather than one because they answer
/// different questions: a depth of 40 items inside 2 purges is a page with a big asset
/// count, while 40 items across 40 purges is a burst of invalidations and a provider that
/// is probably struggling.
#[derive(Debug, Clone, Copy, sqlx::FromRow)]
pub struct QueueDepth {
    /// Items waiting to be attempted.
    pub pending_items: i64,
    /// Purges that have not reached a terminal state.
    pub open_purges: i64,
}

/// Count the queue, optionally for one site.
pub async fn queue_depth(
    pool: &sqlx::PgPool,
    site_id: Option<Uuid>,
) -> Result<QueueDepth, CdnError> {
    sqlx::query_as::<_, QueueDepth>(
        "select \
            (select count(*) from cdn_purge_items i \
              join cdn_purges p on p.id = i.purge_id \
             where i.status = 'pending' and ($1::uuid is null or p.site_id = $1)) as pending_items, \
            (select count(*) from cdn_purges p \
             where p.status in ('queued', 'running') \
               and ($1::uuid is null or p.site_id = $1)) as open_purges",
    )
    .bind(site_id)
    .fetch_one(pool)
    .await
    .map_err(CdnError::from)
}

/// The 24-hour counters the overview shows.
#[derive(Debug, Clone, Copy, sqlx::FromRow)]
pub struct PurgeCounters {
    /// Purges requested in the window.
    pub total: i64,
    /// Of those, how many reached `succeeded`.
    pub succeeded: i64,
    /// Of those, how many ended `partial`.
    pub partial: i64,
    /// Of those, how many ended `failed`.
    pub failed: i64,
}

impl PurgeCounters {
    /// Share of requests that fully failed, as a percentage.
    ///
    /// Computed here rather than in the panel so the panel cannot divide by zero, and so
    /// the "failure rate" on the card is the same number the tests assert. A window with
    /// no purges reports `0.0` rather than `NaN`: there is no failure, and a card showing
    /// `NaN%` reads as a broken panel.
    #[must_use]
    pub fn failure_rate(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        ((self.failed + self.partial) as f64 / self.total as f64) * 100.0
    }
}

/// Count the last `hours` of purges, optionally for one site.
pub async fn counters(
    pool: &sqlx::PgPool,
    site_id: Option<Uuid>,
    hours: i32,
) -> Result<PurgeCounters, CdnError> {
    sqlx::query_as::<_, PurgeCounters>(
        "select \
            count(*) as total, \
            count(*) filter (where status = 'succeeded') as succeeded, \
            count(*) filter (where status = 'partial') as partial, \
            count(*) filter (where status = 'failed') as failed \
         from cdn_purges \
         where requested_at >= now() - make_interval(hours => $2) \
           and ($1::uuid is null or site_id = $1)",
    )
    .bind(site_id)
    .bind(hours.clamp(1, 24 * 30))
    .fetch_one(pool)
    .await
    .map_err(CdnError::from)
}

/// The adapter key, endpoint and zone the worker needs for a site, resolved the same way the
/// panel reads them: the site's own row, else the platform row, else `origin`.
///
/// One query rather than two on purpose. The panel's `store::resolve_settings` and this must
/// agree — a worker that resolved the provider differently from the screen that configured
/// it is how a purge ends up sent to an adapter the operator never chose, and two reads
/// racing a concurrent write is the smaller half of that bug.
pub async fn provider_for_site(
    pool: &sqlx::PgPool,
    site_id: Option<Uuid>,
) -> Result<(String, crate::provider::ProviderSettings, i32), CdnError> {
    let row: Option<(String, Option<String>, Option<String>, i32, i32)> = sqlx::query_as(
        "select provider, endpoint_url, zone_ref, batch_size, max_attempts \
         from cdn_settings \
         where site_id is not distinct from $1 or site_id is null \
         order by site_id is null asc, site_id nulls last \
         limit 1",
    )
    .bind(site_id)
    .fetch_optional(pool)
    .await?;

    Ok(match row {
        Some((key, endpoint, zone, _batch, max_attempts)) => (
            key.clone(),
            // The credential is not read here. `cdn_settings.credential_ciphertext` is
            // write-only by design (REQ-011, "Risks") and slice 4 wires the decrypt; until
            // then an adapter with a stored credential gets `None` and answers honestly
            // that it is not configured, which is the true state of this build.
            crate::provider::ProviderSettings {
                endpoint,
                zone,
                credential: None,
            },
            max_attempts.clamp(1, 10),
        ),
        None => (
            "origin".to_string(),
            crate::provider::ProviderSettings::default(),
            5,
        ),
    })
}

/// The provider's own batch cap, re-exported so a caller need not name two paths.
pub const PROVIDER_BATCH_CAP: usize = MAX_BATCH;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::headers::surrogate_keys;

    fn ts() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH
    }

    fn item(id: i64, target: &str) -> PurgeItemRow {
        PurgeItemRow {
            id,
            purge_id: Uuid::nil(),
            target: target.to_string(),
            status: "pending".into(),
            attempts: 0,
            next_attempt_at: ts(),
            response_status: None,
            error: None,
            done_at: None,
        }
    }

    #[test]
    fn a_url_purge_trims_blanks_and_keeps_the_order_it_was_pasted_in() {
        let targets = validate(
            PurgeKind::Url,
            &[" /a ".into(), String::new(), "/b".into(), "   ".into()],
            false,
        )
        .expect("a list with two real targets is valid");
        assert_eq!(targets, vec!["/a".to_string(), "/b".to_string()]);
    }

    #[test]
    fn a_target_pasted_twice_is_purged_once() {
        let targets = validate(
            PurgeKind::Url,
            &["/a".into(), "/a".into(), "/b".into()],
            false,
        )
        .expect("duplicates are not an error");
        assert_eq!(targets, vec!["/a".to_string(), "/b".to_string()]);
    }

    #[test]
    fn a_url_paste_with_blank_lines_is_not_a_target_over_the_cap() {
        // 500 real targets and 50 blank lines: the user pasted 550 lines and named 500
        // things. Refusing this on the raw line count would be refusing a correct paste.
        let mut raw: Vec<String> = (0..500).map(|n| format!("/page-{n}")).collect();
        raw.extend((0..50).map(|_| String::new()));
        let targets = validate(PurgeKind::Url, &raw, false).expect("500 targets is the cap");
        assert_eq!(targets.len(), MAX_TARGETS);
    }

    #[test]
    fn one_target_over_the_cap_is_refused_with_the_count() {
        let raw: Vec<String> = (0..=MAX_TARGETS).map(|n| format!("/page-{n}")).collect();
        let error = validate(PurgeKind::Url, &raw, false).expect_err("over the cap");
        assert_eq!(
            error,
            PurgeInputError::TooManyTargets {
                given: MAX_TARGETS + 1
            }
        );
        assert_eq!(error.field(), "targets");
    }

    #[test]
    fn a_relative_or_embedded_whitespace_url_is_refused() {
        // Trailing whitespace is trimmed (a paste ends with a newline and that is not an
        // error), but whitespace *inside* a target is: a path containing a space is either
        // a mistyped absolute URL or a smuggled fragment, and a provider will reject it
        // at drain time — which is an hour later instead of now.
        for bad in ["a/b", "/a b", "//evil.example", "/a\tb"] {
            let raw = vec![bad.to_string()];
            assert_eq!(
                validate(PurgeKind::Url, &raw, false).expect_err("must refuse"),
                PurgeInputError::MalformedUrl,
                "{bad:?} must be refused"
            );
        }
    }

    #[test]
    fn a_tag_must_look_like_a_surrogate_key() {
        // The keys the header builder emits must be accepted, or the console would
        // refuse the very tags the platform itself produces.
        for key in surrogate_keys("/blog/post") {
            let raw = vec![key.clone()];
            assert_eq!(
                validate(PurgeKind::Tag, &raw, false).expect("emitted key must be accepted"),
                vec![key.clone()],
                "{key:?} is emitted by the header builder"
            );
        }
        assert_eq!(
            validate(PurgeKind::Tag, &["has space".into()], false).expect_err("must refuse"),
            PurgeInputError::MalformedTag
        );
    }

    #[test]
    fn a_purge_with_no_targets_is_refused_rather_than_succeeding_at_nothing() {
        assert_eq!(
            validate(PurgeKind::Url, &["  ".into()], false).expect_err("must refuse"),
            PurgeInputError::EmptyTargets { kind: "url" }
        );
    }

    #[test]
    fn the_whole_zone_needs_the_word_purge_typed() {
        assert_eq!(
            validate(PurgeKind::All, &[], false).expect_err("unconfirmed"),
            PurgeInputError::AllNeedsConfirmation
        );
        assert_eq!(
            validate(PurgeKind::All, &[], true).expect("confirmed"),
            vec!["*".to_string()]
        );
    }

    #[test]
    fn a_target_list_splits_into_provider_batches_of_the_caps_size() {
        let targets: Vec<String> = (0..MAX_BATCH + 7).map(|n| format!("/p-{n}")).collect();
        let batches = batches(PurgeKind::Url, &targets);
        assert_eq!(batches.len(), 2, "500 + 7 is two calls");
        match &batches[0] {
            Purge::Urls { targets } => assert_eq!(targets.len(), MAX_BATCH),
            other => panic!("expected a url batch, got {other:?}"),
        }
    }

    #[test]
    fn a_purge_that_fits_the_cap_is_exactly_one_call() {
        let targets: Vec<String> = (0..10).map(|n| format!("/p-{n}")).collect();
        assert_eq!(batches(PurgeKind::Url, &targets).len(), 1);
        assert_eq!(batches(PurgeKind::All, &["*".to_string()]).len(), 1);
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        assert!(backoff_seconds(0, 1) <= backoff_seconds(4, 1));
        assert!(backoff_seconds(0, 1) <= 300);
        assert!(backoff_seconds(20, 7) <= 300, "the cap holds at any attempt count");
    }

    #[test]
    fn backoff_is_never_negative_whatever_the_salt() {
        for salt in [0, 1, -1, i64::MIN, i64::MAX, 4_999_999] {
            for attempts in 0..12 {
                assert!(
                    backoff_seconds(attempts, salt) >= 0,
                    "attempt {attempts} salt {salt}"
                );
            }
        }
    }

    #[test]
    fn two_items_failing_together_do_not_get_the_same_delay() {
        // The same attempt number, two different item ids: identical backoff is the
        // thundering herd this exists to prevent.
        let first = backoff_seconds(3, 1);
        let second = backoff_seconds(3, 2);
        assert_ne!(first, second);
    }

    #[test]
    fn the_same_item_at_the_same_attempt_gets_the_same_delay() {
        // A worker restart mid-retry must not reshuffle the schedule it is following.
        assert_eq!(backoff_seconds(3, 42), backoff_seconds(3, 42));
    }

    #[test]
    fn a_purge_with_an_item_still_in_flight_is_not_succeeded_yet() {
        let (status, failed) = summarise(&[ItemStatus::Done, ItemStatus::Pending]);
        assert_eq!(status, PurgeStatus::Running);
        assert_eq!(failed, 0);
    }

    #[test]
    fn all_done_is_succeeded_and_all_failed_is_failed_and_a_mix_is_partial() {
        assert_eq!(
            summarise(&[ItemStatus::Done, ItemStatus::Done]).0,
            PurgeStatus::Succeeded
        );
        assert_eq!(
            summarise(&[ItemStatus::Failed, ItemStatus::Failed]).0,
            PurgeStatus::Failed
        );
        let (status, failed) = summarise(&[ItemStatus::Done, ItemStatus::Failed]);
        assert_eq!(status, PurgeStatus::Partial);
        assert_eq!(failed, 1);
    }

    #[test]
    fn a_failure_inside_the_attempt_budget_goes_back_to_pending_not_failed() {
        let mut items = vec![item(1, "/a")];
        let (failed, message) = apply_outcome(
            &mut items,
            &PurgeOutcome::Failed {
                message: "provider is down".into(),
            },
            ts(),
            5,
            0,
        );
        assert_eq!(failed, 0, "one failure of five attempts is not a failed item");
        assert_eq!(items[0].status, "pending");
        assert_eq!(items[0].attempts, 1);
        assert_eq!(message.as_deref(), Some("provider is down"));
    }

    #[test]
    fn an_item_exhausting_its_budget_becomes_failed() {
        let mut items = vec![item(1, "/a")];
        items[0].attempts = 4;
        let (failed, _) = apply_outcome(
            &mut items,
            &PurgeOutcome::Failed {
                message: "still down".into(),
            },
            ts(),
            5,
            0,
        );
        assert_eq!(failed, 1);
        assert_eq!(items[0].status, "failed");
        assert_eq!(items[0].done_at, Some(ts()));
    }

    #[test]
    fn a_partial_outcome_fails_only_the_targets_the_provider_refused() {
        let mut items = vec![item(1, "/a"), item(2, "/b"), item(3, "/c")];
        let (failed, _) = apply_outcome(
            &mut items,
            &PurgeOutcome::Partial {
                failed: vec!["/b".into()],
                message: "one target was not found".into(),
            },
            ts(),
            1,
            0,
        );
        assert_eq!(failed, 1);
        assert_eq!(items[0].status, "done");
        assert_eq!(items[1].status, "failed");
        assert_eq!(items[2].status, "done");
    }

    #[test]
    fn the_message_shown_comes_from_a_failing_item_not_the_parents_summary() {
        let mut items = vec![item(1, "/a"), item(2, "/b")];
        items[1].error = Some("zone quota exceeded".into());
        assert_eq!(
            failure_message(&items, Some("parent said something generic")),
            Some("zone quota exceeded".to_string())
        );
    }

    #[test]
    fn an_empty_window_reports_a_zero_failure_rate_rather_than_nan() {
        let counters = PurgeCounters {
            total: 0,
            succeeded: 0,
            partial: 0,
            failed: 0,
        };
        assert_eq!(counters.failure_rate(), 0.0);
        assert!(counters.failure_rate().is_finite());
    }

    #[test]
    fn a_partial_counts_towards_the_failure_rate_because_a_page_is_still_stale() {
        // Half the targets went through: the operator's page is still wrong for some
        // visitors, so a card showing 0% here would be reporting the wrong thing.
        let counters = PurgeCounters {
            total: 2,
            succeeded: 1,
            partial: 1,
            failed: 0,
        };
        assert!((counters.failure_rate() - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn only_a_failed_or_partial_purge_offers_a_retry() {
        assert!(PurgeStatus::Failed.retryable());
        assert!(PurgeStatus::Partial.retryable());
        assert!(!PurgeStatus::Succeeded.retryable());
        assert!(!PurgeStatus::Running.retryable());
        assert!(!PurgeStatus::Queued.retryable());
    }

    #[test]
    fn the_kind_round_trips_through_its_wire_form() {
        for kind in [PurgeKind::Url, PurgeKind::Tag, PurgeKind::All] {
            assert_eq!(PurgeKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(PurgeKind::parse("everything"), None);
    }
}
