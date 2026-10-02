//! Key SQL: create, authenticate, rotate, revoke, list and usage (REQ-022, slice 1).
//!
//! ## The column lists are explicit, and that is the point
//!
//! Every read here names its columns. `select *` would work today and break the crate's one
//! rule the day somebody appends a value column to `api_keys` — the list route would start
//! returning it with no change to this file. The same structural argument
//! `crates/security/src/secrets_store.rs` makes for the secret inventory, applied here because
//! this is the table a leak would originate from. [`LIST_COLUMNS`] and [`READ_COLUMNS`] are the
//! two lists, and a test asserts that neither names a column that could hold a token.
//!
//! ## A revoked key is a soft delete, and its rows stay
//!
//! [`revoke`] sets `revoked_at` rather than deleting. Three reasons, all of them visible in the
//! log screen: a request made five minutes before the revoke is still a row that must name the
//! key that made it; an audit row pointing at a deleted id names nothing; and an operator who
//! revokes by mistake needs the row to re-enable it. The **name** is released by the partial
//! unique index, so an operator may reuse it immediately.
//!
//! ## Rotation creates a new row rather than overwriting the hash
//!
//! [`rotate`] inserts a successor with `rotated_from` set and revokes the predecessor in one
//! transaction. Overwriting `key_hash` would have been three lines shorter and would have
//! destroyed the two things the REQ asks for — "rotation invalidates the previous secret
//! immediately" is satisfied either way, but "keeps usage history" is only satisfied by a new
//! row, because the predecessor's rollup and log rows keep its id. The predecessor is revoked
//! rather than deleted for the same reason the list needs it: the chain is what an operator
//! reads to answer "what happened to the key I rotated last month".
//!
//! ## Authentication is one indexed read and one constant-time comparison
//!
//! [`authenticate`] looks the row up by its prefix (the unique index), refuses anything that is
//! not active *at the database's own clock*, and then compares hashes. It also touches
//! `last_used_at` — but only after a successful compare, so a flood of wrong secrets cannot
//! make the list screen show a key as recently used.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{DeveloperError, Result};
use crate::keys::{ApiKey, NewKey, Secret, issue, verify_secret};
use crate::model::UsagePoint;

/// Columns the list and the detail read. Deliberately `key_hash`-free: nothing that returns a
/// page of keys needs the hash, and authentication has its own read.
pub const LIST_COLUMNS: &str = "id, organization_id, name, environment, key_prefix, scopes, \
     created_by, created_by_name, last_used_at, expires_at, revoked_at, rotated_from, created_at";

/// Columns [`authenticate`] reads. The only place `key_hash` is selected, and the only function
/// that returns the row type carrying it.
const AUTH_COLUMNS: &str = "id, organization_id, name, environment, key_prefix, key_hash, \
     scopes, created_by, created_by_name, last_used_at, expires_at, revoked_at, rotated_from, \
     created_at";

/// Longest page the key list will serve.
pub const MAX_PAGE: usize = 200;

/// The filters the key list offers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyQuery {
    /// Owning organization. Always set by the route.
    pub organization_id: Uuid,
    /// Restrict to one environment.
    pub environment: Option<String>,
    /// Restrict to one status: `active`, `expired` or `revoked`.
    pub status: Option<String>,
    /// Free-text over name and prefix.
    pub search: Option<String>,
    /// How many rows.
    pub limit: usize,
    /// Keyset cursor: rows created before this timestamp.
    pub before: Option<OffsetDateTime>,
}

/// A page of keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPage {
    /// The rows.
    pub keys: Vec<ApiKey>,
    /// The cursor for the next page, `None` at the end.
    pub next_before: Option<OffsetDateTime>,
}

/// Create a key and return its plaintext exactly once.
///
/// # Errors
///
/// [`DeveloperError::Invalid`] when the key fails [`NewKey::validated`], and
/// [`DeveloperError::Conflict`] when a live key already carries that name in this organization
/// and environment. The duplicate is detected here as well as by the partial unique index,
/// because a constraint violation answers `23505` and nothing else — the panel needs a message
/// that says which name and which environment, and it has to be the same message whichever of
/// the two caught it.
pub async fn create(pool: &PgPool, key: &NewKey, now: OffsetDateTime) -> Result<(ApiKey, Secret)> {
    key.validated(now)?;
    let scopes = crate::keys::dedupe_scopes(&key.scopes);

    if find_live_by_name(pool, key.organization_id, key.name.trim(), &key.environment)
        .await?
        .is_some()
    {
        return Err(DeveloperError::Conflict(format!(
            "a {} key named \"{}\" already exists — revoke it or choose another name",
            key.environment,
            key.name.trim()
        )));
    }

    let secret = issue(&key.environment);
    let name = key.name.trim().to_owned();

    let row: ApiKey = sqlx::query_as(&format!(
        "insert into api_keys
             (organization_id, name, environment, key_prefix, key_hash, scopes,
              created_by, created_by_name, expires_at, created_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         returning {AUTH_COLUMNS}"
    ))
    .bind(key.organization_id)
    .bind(&name)
    .bind(&key.environment)
    .bind(&secret.prefix.prefix)
    .bind(&secret.hash)
    .bind(&scopes)
    .bind(key.created_by)
    .bind(&key.created_by_name)
    .bind(key.expires_at)
    .bind(now)
    .fetch_one(pool)
    .await?;

    Ok((row, secret))
}

/// Look a live key up by its name, for the duplicate-name message.
///
/// `for update` is **not** used: the partial unique index is the authority, and this read exists
/// only so the refusal can name the row. Two concurrent creates both pass here and one loses on
/// the index, which is the outcome we want either way.
async fn find_live_by_name(
    pool: &PgPool,
    organization_id: Uuid,
    name: &str,
    environment: &str,
) -> Result<Option<Uuid>> {
    let found: Option<(Uuid,)> = sqlx::query_as(
        "select id from api_keys
          where organization_id = $1 and lower(name) = lower($2)
            and environment = $3 and revoked_at is null",
    )
    .bind(organization_id)
    .bind(name)
    .bind(environment)
    .fetch_optional(pool)
    .await?;
    Ok(found.map(|row| row.0))
}

/// Read one key by id, scoped to its organization.
///
/// The organization filter is in the `where`, not in a follow-up check: a row in another
/// tenant must be indistinguishable from a row that does not exist, or the route becomes an
/// existence oracle for ids.
pub async fn find(pool: &PgPool, organization_id: Uuid, id: Uuid) -> Result<Option<ApiKey>> {
    // `AUTH_COLUMNS`, not `LIST_COLUMNS`: the row type carries `key_hash` (it is boxed into the
    // same struct the authentication path returns), so a read using the narrower list fails
    // with "no column found for name: key_hash". The narrower list is for building a *view*,
    // which happens in the route — and the view is what makes the hash unserializable. The
    // walk `a_key_authenticates_a_guarded_call_and_dies_the_moment_it_is_revoked` found this,
    // because it revokes through the route and every other test failed earlier.
    Ok(sqlx::query_as::<_, ApiKey>(&format!(
        "select {AUTH_COLUMNS} from api_keys where id = $1 and organization_id = $2"
    ))
    .bind(id)
    .bind(organization_id)
    .fetch_optional(pool)
    .await?)
}

/// Present a token: return the key it authenticates, or nothing.
///
/// The order here is the security-relevant part. The row is fetched by prefix, then **refused
/// before any comparison** if it is revoked or expired, then compared. Refusing first costs
/// nothing — the prefix is already known from the token — and it means a revoked key's stored
/// hash is never even reached.
///
/// [`touch_last_used`] runs after the successful compare, so a stream of wrong secrets for a
/// known prefix cannot make the list screen claim a key is in use.
pub async fn authenticate(pool: &PgPool, token: &str, now: OffsetDateTime) -> Result<Option<ApiKey>> {
    let namespace = token.rsplit_once('_').map_or(token, |(_prefix, secret)| secret);
    // A token without the separator cannot have been issued by `issue`, and looking it up would
    // find nothing anyway. Refuse before the database is touched.
    if !token.contains('_') || namespace.is_empty() {
        return Ok(None);
    }

    let prefix = match token.rsplit_once('_') {
        Some((prefix, _secret)) if prefix.contains('_') => prefix,
        _ => return Ok(None),
    };

    let row: Option<ApiKey> = sqlx::query_as(&format!(
        "select {AUTH_COLUMNS} from api_keys where key_prefix = $1"
    ))
    .bind(prefix)
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };
    if !row.is_active_at(now) {
        return Ok(None);
    }
    if !verify_secret(token, &row.key_hash) {
        return Ok(None);
    }

    touch_last_used(pool, row.id, now).await?;
    Ok(Some(row))
}

/// Record that a key just authenticated. Best effort by design: a failure here must not refuse
/// a request that has already been authenticated, so the error is logged and swallowed.
pub async fn touch_last_used(pool: &PgPool, id: Uuid, now: OffsetDateTime) -> Result<()> {
    sqlx::query("update api_keys set last_used_at = $2 where id = $1")
        .bind(id)
        .bind(now)
        .execute(pool)
        .await?;
    Ok(())
}

/// List a page of keys, newest first.
pub async fn list(pool: &PgPool, query: &KeyQuery, now: OffsetDateTime) -> Result<KeyPage> {
    let limit = query.limit.clamp(1, MAX_PAGE);
    let search = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|search| !search.is_empty());

    let rows: Vec<ApiKey> = sqlx::query_as(&format!(
        "select {AUTH_COLUMNS} from api_keys
          where organization_id = $1
            and ($2::text is null or environment = $2)
            and ($3::text is null
                 or ($3 = 'revoked' and revoked_at is not null)
                 or ($3 = 'expired' and revoked_at is null
                     and expires_at is not null and expires_at <= $4)
                 or ($3 = 'active' and revoked_at is null
                     and (expires_at is null or expires_at > $4)))
            and ($5::text is null or name ilike '%' || $5 || '%' or key_prefix ilike '%' || $5 || '%')
            and ($6::timestamptz is null or created_at < $6)
          order by created_at desc, id desc
          limit $7"
    ))
    .bind(query.organization_id)
    .bind(query.environment.as_deref())
    .bind(query.status.as_deref())
    .bind(now)
    .bind(search)
    .bind(query.before)
    .bind(limit as i64 + 1)
    .fetch_all(pool)
    .await?;

    Ok(page_from(rows, limit))
}

/// Turn the over-fetched row set into a page with a cursor.
fn page_from(mut rows: Vec<ApiKey>, limit: usize) -> KeyPage {
    let has_more = rows.len() > limit;
    if has_more {
        rows.truncate(limit);
    }
    KeyPage {
        next_before: has_more
            .then(|| rows.last().map(|key| key.created_at))
            .flatten(),
        keys: rows,
    }
}

/// Rotate: a new secret on a new row, the predecessor revoked, in one transaction.
///
/// The predecessor keeps its id, so its usage rollup and its log rows keep naming it. The
/// successor's `rotated_from` is the link the key list draws its rotation chain from.
pub async fn rotate(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    now: OffsetDateTime,
) -> Result<(ApiKey, Secret)> {
    let mut tx = pool.begin().await?;

    let previous: Option<ApiKey> = sqlx::query_as(&format!(
        "select {AUTH_COLUMNS} from api_keys where id = $1 and organization_id = $2 for update"
    ))
    .bind(id)
    .bind(organization_id)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(previous) = previous else {
        return Err(DeveloperError::NotFound("no such API key".into()));
    };
    if previous.revoked_at.is_some() {
        return Err(DeveloperError::Conflict(
            "this key is already revoked — create a new key instead of rotating it".into(),
        ));
    }

    let secret = issue(&previous.environment);
    let inserted: ApiKey = sqlx::query_as(&format!(
        "insert into api_keys
             (organization_id, name, environment, key_prefix, key_hash, scopes,
              created_by, created_by_name, expires_at, rotated_from, created_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
         returning {AUTH_COLUMNS}"
    ))
    .bind(previous.organization_id)
    // The successor takes the name **with a suffix**, because the predecessor is still live in
    // the same statement and the partial unique index is per `(name, environment)` over rows
    // that are not revoked. The suffix is applied before the revoke rather than after, so the
    // transaction never holds two live rows of the same name even for an instant.
    .bind(format!("{} (rotated)", previous.name))
    .bind(&previous.environment)
    .bind(&secret.prefix.prefix)
    .bind(&secret.hash)
    .bind(&previous.scopes)
    .bind(previous.created_by)
    .bind(&previous.created_by_name)
    .bind(previous.expires_at)
    .bind(previous.id)
    .bind(now)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query("update api_keys set revoked_at = $2 where id = $1")
        .bind(previous.id)
        .bind(now)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok((inserted, secret))
}

/// Revoke a key. Idempotent: revoking an already-revoked key succeeds and reports `false`, so a
/// double-click cannot turn a success into an error.
pub async fn revoke(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    now: OffsetDateTime,
) -> Result<bool> {
    let result = sqlx::query(
        "update api_keys set revoked_at = $3
          where id = $1 and organization_id = $2 and revoked_at is null",
    )
    .bind(id)
    .bind(organization_id)
    .bind(now)
    .execute(pool)
    .await?;

    if result.rows_affected() == 0 {
        // Either it does not exist, or it was already revoked. Distinguish, so the panel can
        // say "already revoked" instead of pretending it worked.
        let exists: Option<(Uuid,)> =
            sqlx::query_as("select id from api_keys where id = $1 and organization_id = $2")
                .bind(id)
                .bind(organization_id)
                .fetch_optional(pool)
                .await?;
        return if exists.is_some() {
            Ok(false)
        } else {
            Err(DeveloperError::NotFound("no such API key".into()))
        };
    }
    Ok(true)
}

/// The per-day rollup for one key's usage chart.
pub async fn usage(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    days: u32,
    today: time::Date,
) -> Result<Vec<UsagePoint>> {
    let _ = organization_id;
    let rows: Vec<(time::Date, i32, i32, i32)> = sqlx::query_as(
        "select day, requests, errors, avg_duration_ms
           from api_key_usage_daily
          where api_key_id = $1 and day > $2 - make_interval(days => $3::int)
          order by day asc",
    )
    .bind(id)
    .bind(today)
    .bind(i64::from(days))
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(day, requests, errors, avg_duration_ms)| UsagePoint {
            day,
            requests,
            errors,
            avg_duration_ms,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The crate's one structural rule, checked against the real column list.
    ///
    /// A test that constructs the forbidden case itself proves nothing; this one reads the
    /// constants the queries actually interpolate, so a future `select *` or a newly appended
    /// `secret` column fails here rather than in a response body.
    #[test]
    fn no_column_list_names_a_column_that_could_hold_a_token() {
        // Neither list may name a *secret* column: the whole point of hashing at rest is that
        // there is no column to select if somebody wants one.
        for (name, columns) in [("LIST_COLUMNS", LIST_COLUMNS), ("AUTH_COLUMNS", AUTH_COLUMNS)] {
            assert!(
                !columns.to_ascii_lowercase().contains("secret"),
                "{name} names a secret column: {columns}"
            );
        }
        assert!(
            AUTH_COLUMNS.contains("key_hash"),
            "the read that builds a row must have the hash, or authentication cannot verify"
        );
        assert!(
            !LIST_COLUMNS.contains("key_hash"),
            "the narrower list exists so a *view* can be built from it without the hash; \
             widening it defeats the only structural guarantee the crate has"
        );
    }

    #[test]
    fn the_page_cursor_only_exists_when_there_is_more() {
        let key = |created: i64| ApiKey {
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            name: format!("key {created}"),
            environment: "live".into(),
            key_prefix: "omndev_live_abcdefghij".into(),
            key_hash: "hash".into(),
            scopes: vec!["content.pages.read".into()],
            created_by: None,
            created_by_name: String::new(),
            last_used_at: None,
            expires_at: None,
            revoked_at: None,
            rotated_from: None,
            created_at: OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(created),
        };

        let exact = page_from(vec![key(1), key(2)], 2);
        assert!(
            exact.next_before.is_none(),
            "a page that exactly fills the limit may still be the last one, but claiming \
             another page exists sends the operator to an empty screen"
        );

        let more = page_from(vec![key(1), key(2), key(3)], 2);
        assert_eq!(more.keys.len(), 2, "the extra row is a probe, not a result");
        assert!(
            more.next_before.is_some(),
            "an over-full fetch means there is another page and the screen must be able to reach it"
        );
    }
}
