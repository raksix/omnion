//! The database half: reading and writing keys, their usage and the request log.
//!
//! Every function here takes the organization's id as a parameter rather than reading it from
//! a session, so the tenant boundary is the caller's argument and a query that forgot to filter
//! is a compile error at the call site instead of a cross-tenant read at runtime.
//!
//! The store is behind the `store` feature, as `omnion-deployment`'s is: this crate's secret
//! and model logic is worth testing with no database at all, and a test module that needs a
//! `PgPool` to check a name length is a test that cannot run on a busy box.

use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{DeveloperError, Result};
use crate::model::{
    ApiKey, Environment, KeyStatus, Minted, NewKey, RateTier, RequestLog, RequestLogPage,
    RequestLogQuery, UsageDay, key_rules,
};
use crate::secret;

/// The columns every key read returns, in one place.
///
/// Written out rather than `select *` so that adding a column to the table cannot silently
/// change a response shape, and so the mapping below is the only place that knows the order.
const KEY_COLUMNS: &str = "id, organization_id, name, prefix, scopes, environment, rate_tier, \
     ip_allowlist, expires_at, last_used_at, revoked_at, created_by, created_at, rotated_at";

/// Turn a row into the shape the API returns, computing the status rather than reading it.
async fn key_from_row(row: sqlx::postgres::PgRow) -> Result<ApiKey> {
    let organization_id: Uuid = row.try_get("organization_id")?;
    let scopes: serde_json::Value = row.try_get("scopes")?;
    let environment: String = row.try_get("environment")?;
    let rate_tier: String = row.try_get("rate_tier")?;
    let ip_allowlist: Option<serde_json::Value> = row.try_get("ip_allowlist")?;
    let expires_at: Option<OffsetDateTime> = row.try_get("expires_at")?;
    let last_used_at: Option<OffsetDateTime> = row.try_get("last_used_at")?;
    let revoked_at: Option<OffsetDateTime> = row.try_get("revoked_at")?;
    let rotated_at: Option<OffsetDateTime> = row.try_get("rotated_at")?;

    Ok(ApiKey {
        id: row.try_get("id")?,
        organization_id,
        name: row.try_get("name")?,
        prefix: row.try_get("prefix")?,
        scopes: string_array(scopes),
        environment: Environment::parse(&environment)?,
        rate_tier: RateTier::parse(&rate_tier)?,
        ip_allowlist: ip_allowlist.map(|value| string_array(value)),
        expires_at,
        last_used_at,
        revoked_at,
        rotated_at,
        created_by: row.try_get("created_by")?,
        created_at: row.try_get("created_at")?,
        status: KeyStatus::of(revoked_at, expires_at, OffsetDateTime::now_utc()),
    })
}

/// Read a `jsonb` array of strings, tolerating a non-array by returning nothing rather than
/// failing a whole list read over one bad row.
fn string_array(value: serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Validate a submitted IP allowlist.
///
/// Each entry must parse as a network. An empty *list* is refused rather than stored as "any":
/// a UI that submits no rows means "no restriction" and expresses that by omitting the field,
/// so an explicit empty array is a question we cannot read the intent behind.
pub fn validate_ip_allowlist(entries: &Option<Vec<String>>) -> Result<Option<Vec<String>>> {
    let Some(entries) = entries else {
        return Ok(None);
    };
    if entries.is_empty() {
        return Ok(None);
    }
    for entry in entries {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !looks_like_cidr(trimmed) {
            return Err(DeveloperError::InvalidCidr(entry.clone()));
        }
    }
    let cleaned: Vec<String> = entries
        .iter()
        .map(|entry| entry.trim().to_owned())
        .filter(|entry| !entry.is_empty())
        .collect();

    // Checked *after* the blanks are dropped, not before: a list of nothing but blank rows is
    // what a form with one empty input submits, and storing that as an empty allowlist would
    // lock the owner out of their own key with no address on the panel to explain why. It
    // means "no restriction" — the same reading as omitting the field.
    if cleaned.is_empty() {
        return Ok(None);
    }
    Ok(Some(cleaned))
}

/// A cheap structural check on a CIDR entry.
///
/// This is a *shape* check, not a parse: the address family and the prefix length are checked
/// because a typo there is the common case, and the platform does not link an IP-parsing
/// dependency to a table that only ever stores the strings and compares them at the edge.
fn looks_like_cidr(entry: &str) -> bool {
    let Some((address, prefix)) = entry.rsplit_once('/') else {
        return false;
    };
    let Ok(bits) = prefix.parse::<u8>() else {
        return false;
    };

    match address.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(_)) => bits <= 32,
        Ok(std::net::IpAddr::V6(_)) => bits <= 128,
        Err(_) => false,
    }
}

/// Create a key, returning the plaintext exactly once.
///
/// The whole operation is one transaction, and the secret is hashed *before* the insert, so a
/// failure cannot leave a row whose hash we never computed — which would be a key that exists,
/// appears in the panel, and can never be used.
pub async fn create(pool: &PgPool, organization_id: Uuid, new_key: &NewKey) -> Result<Minted> {
    key_rules::validate_name(&new_key.name)?;
    key_rules::validate_scopes(&new_key.scopes)?;
    let ip_allowlist = validate_ip_allowlist(&new_key.ip_allowlist)?;

    let minted = secret::mint();
    // Split once here and never re-derive it: the plaintext is the concatenation, and the
    // hash covers the secret half only, because that is the half a caller proves.
    let (_, secret_half) =
        secret::split_token(&minted.plaintext).ok_or(DeveloperError::KeyUnverifiable)?;
    let stored_hash = secret::hash(secret_half);

    let mut transaction = pool.begin().await?;

    let row = sqlx::query(&format!(
        "insert into api_keys (organization_id, name, prefix, secret_hash, scopes, environment, \
         rate_tier, ip_allowlist, expires_at, created_by) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
         returning {KEY_COLUMNS}"
    ))
    .bind(organization_id)
    .bind(new_key.name.trim())
    .bind(&minted.prefix)
    .bind(&stored_hash)
    .bind(serde_json::json!(new_key.scopes))
    .bind(new_key.environment.as_str())
    .bind(new_key.rate_tier.as_str())
    .bind(ip_allowlist.map(|entries| serde_json::json!(entries)))
    .bind(new_key.expires_at)
    .bind(new_key.created_by)
    .fetch_one(&mut *transaction)
    .await
    // A unique-index violation on the name is the one database error worth translating: the
    // panel needs to say "that name is taken" next to the field rather than showing a 500.
    .map_err(map_insert_error)?;

    transaction.commit().await?;

    Ok(Minted {
        key: key_from_row(row).await?,
        plaintext: minted.plaintext,
    })
}

/// Translate the two unique-index violations a key insert can hit.
fn map_insert_error(error: sqlx::Error) -> DeveloperError {
    if let sqlx::Error::Database(ref inner) = error {
        // 23505 is unique_violation. The index name says which constraint, so a name clash
        // and a prefix collision get different messages — the prefix one is astronomically
        // unlikely and reads as a bug when it happens, which it is.
        if inner.code().as_deref() == Some("23505") {
            if inner.constraint() == Some("api_keys_org_name_key") {
                return DeveloperError::KeyNameTaken(String::new());
            }
        }
    }
    DeveloperError::Database(error)
}

/// List an organization's keys, newest first.
pub async fn list(pool: &PgPool, organization_id: Uuid) -> Result<Vec<ApiKey>> {
    let rows = sqlx::query(&format!(
        "select {KEY_COLUMNS} from api_keys where organization_id = $1 order by created_at desc, id"
    ))
    .bind(organization_id)
    .fetch_all(pool)
    .await?;

    let mut keys = Vec::with_capacity(rows.len());
    for row in rows {
        keys.push(key_from_row(row).await?);
    }
    Ok(keys)
}

/// Read one key, scoped to its organization.
///
/// The organization is part of the `where` rather than checked afterwards, so a key belonging
/// to somebody else is not found — the same answer as a key that does not exist, and not a 403
/// that confirms it does.
pub async fn get(pool: &PgPool, organization_id: Uuid, key_id: Uuid) -> Result<ApiKey> {
    let row = sqlx::query(&format!(
        "select {KEY_COLUMNS} from api_keys where organization_id = $1 and id = $2"
    ))
    .bind(organization_id)
    .bind(key_id)
    .fetch_optional(pool)
    .await?
    .ok_or(DeveloperError::KeyNotFound)?;

    key_from_row(row).await
}

/// Rotate a key: a new secret, the old one dead immediately.
///
/// Rotation deliberately has **no** overlap window, unlike the OAuth client secret in slice 3
/// where the request asks for one. A key is a single credential in one caller's configuration;
/// an overlap would mean two live values for one row, and the second read of this function
/// would have to guess which one a request authenticated with. The UI states that the old
/// secret stops working at once, because that is the behaviour and the surprise is the cost of
/// saying so plainly.
pub async fn rotate(
    pool: &PgPool,
    organization_id: Uuid,
    key_id: Uuid,
    now: OffsetDateTime,
) -> Result<Minted> {
    let minted = secret::mint();
    let (_, secret_half) =
        secret::split_token(&minted.plaintext).ok_or(DeveloperError::KeyUnverifiable)?;
    let stored_hash = secret::hash(secret_half);

    let mut transaction = pool.begin().await?;

    // The `and revoked_at is null` is in the update, not in a read first: two rotations
    // racing would otherwise both read "active" and the second would silently win.
    let row = sqlx::query(&format!(
        "update api_keys set secret_hash = $3, rotated_at = $4 \
         where organization_id = $1 and id = $2 and revoked_at is null \
         returning {KEY_COLUMNS}"
    ))
    .bind(organization_id)
    .bind(key_id)
    .bind(&stored_hash)
    .bind(now)
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or(DeveloperError::KeyNotFound)?;

    transaction.commit().await?;

    Ok(Minted {
        key: key_from_row(row).await?,
        plaintext: minted.plaintext,
    })
}

/// Revoke a key. Idempotent: revoking twice is not an error, it is the same end state.
pub async fn revoke(
    pool: &PgPool,
    organization_id: Uuid,
    key_id: Uuid,
    now: OffsetDateTime,
) -> Result<ApiKey> {
    let row = sqlx::query(&format!(
        "update api_keys set revoked_at = coalesce(revoked_at, $3) \
         where organization_id = $1 and id = $2 \
         returning {KEY_COLUMNS}"
    ))
    .bind(organization_id)
    .bind(key_id)
    .bind(now)
    .fetch_optional(pool)
    .await?
    .ok_or(DeveloperError::KeyNotFound)?;

    key_from_row(row).await
}

/// Look a key up by its public prefix, for the authentication path.
///
/// This is the one read that is not organization-scoped, because a caller presents a key
/// before the platform knows who they are. It returns the row *and* the organization, so the
/// caller can then check the token against the scopes of the tenant it belongs to rather than
/// trusting a key row it found by string match.
pub async fn find_by_prefix(pool: &PgPool, prefix: &str) -> Result<Option<(ApiKey, Uuid)>> {
    let row = sqlx::query(&format!(
        "select {KEY_COLUMNS}, organization_id as owning_organization \
         from api_keys where prefix = $1"
    ))
    .bind(prefix)
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };
    let owning_organization: Uuid = row.try_get("owning_organization")?;
    Ok(Some((key_from_row(row).await?, owning_organization)))
}

/// Record that a key authenticated a request, and bump its daily rollup.
///
/// `last_used_at` is advanced to the later of the stored value and this instant rather than
/// overwritten, so a request whose clock reads a moment early cannot move the column
/// backwards and make the key look unused after it plainly was.
pub async fn record_use(
    pool: &PgPool,
    key_id: Uuid,
    organization_id: Uuid,
    status: i16,
    duration_ms: i32,
    now: OffsetDateTime,
) -> Result<()> {
    let mut transaction = pool.begin().await?;

    sqlx::query(
        "update api_keys set last_used_at = greatest(coalesce(last_used_at, $3), $3) where id = $1",
    )
    .bind(key_id)
    .bind(organization_id)
    .bind(now)
    .execute(&mut *transaction)
    .await?;

    // The rollup is updated in the same transaction as the log row, so the chart and the
    // history can never disagree about whether a request happened. `errors` counts a
    // response that was not 2xx — a 404 from a key with the wrong scope is a request that
    // happened, and hiding it would make the chart flatter than the truth.
    let day = now.date();
    let failed = i32::from(status < 200 || status >= 300);
    sqlx::query(
        "insert into api_key_usage_daily (api_key_id, day, requests, errors) \
         values ($1, $2, 1, $3) \
         on conflict (api_key_id, day) do update set \
             requests = api_key_usage_daily.requests + 1, \
             errors = api_key_usage_daily.errors + excluded.errors, \
             p95_ms = greatest(coalesce(api_key_usage_daily.p95_ms, 0), $4)",
    )
    .bind(key_id)
    .bind(day)
    .bind(failed)
    .bind(duration_ms)
    .execute(&mut *transaction)
    .await?;

    transaction.commit().await?;
    Ok(())
}

/// One key's daily usage, oldest first, for the chart on the key's detail screen.
pub async fn usage(
    pool: &PgPool,
    organization_id: Uuid,
    key_id: Uuid,
    days: i64,
) -> Result<Vec<UsageDay>> {
    // Bounded rather than open: a chart that renders ten thousand points is a chart nobody
    // reads, and the request asks for daily bars.
    let days = days.clamp(1, 365);
    let rows = sqlx::query_as::<_, UsageDay>(
        "select u.day, u.requests, u.errors, u.p95_ms from api_key_usage_daily u \
         join api_keys k on k.id = u.api_key_id \
         where k.organization_id = $1 and u.api_key_id = $2 \
         and u.day >= current_date - $3::int \
         order by u.day asc",
    )
    .bind(organization_id)
    .bind(key_id)
    .bind(days as i32)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Strip the query string (and any fragment) from a request path.
///
/// This is the property the request log table depends on, and it lives beside the writer rather
/// than in `model` because that is the only place that can guarantee it: a helper the callers
/// *may* use is a rule the next writer has to remember, and the thing it protects is a secret in
/// a CSV export.
///
/// `?a=1#frag` loses the fragment too. A fragment never reaches the server in the first place, so
/// a path that carries one is being constructed somewhere else — and stripping it is the same
/// answer there as leaving it.
pub fn path_without_query(path: &str) -> String {
    let before_fragment = path.split('#').next().unwrap_or(path);
    before_fragment
        .split('?')
        .next()
        .unwrap_or(before_fragment)
        .to_owned()
}

/// Write one request log row.
///
/// No body, no headers, no query string — the columns are the whole of what the platform knows
/// about a request once it is over, and a caller cannot get more detail out of this than they
/// put in the path.
///
/// The query string is stripped **here**, at the last point where the raw path still exists, and
/// not in the caller. A query string is caller-controlled and routinely carries a token or an
/// e-mail address in a filter, and this is the table an operator exports to CSV. Stripping it in
/// `developer_auth::record_key_use` worked and was still the wrong place: it left a writer that
/// binds `entry.path` verbatim, so the next caller — the CSV exporter, a replay, a future
/// middleware — writes the secret unless it remembers a rule that is not on its signature. The
/// strip is a property of the table, so it belongs in the only function that inserts into it.
pub async fn log_request(pool: &PgPool, entry: &RequestLog) -> Result<i64> {
    let path = path_without_query(&entry.path);
    let row = sqlx::query(
        "insert into api_request_logs (organization_id, api_key_id, actor_user_id, method, path, \
         status, duration_ms, request_id, bytes_in, bytes_out, error_code) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) returning id",
    )
    .bind(entry.organization_id)
    .bind(entry.api_key_id)
    .bind(entry.actor_user_id)
    .bind(&entry.method)
    .bind(&path)
    .bind(entry.status)
    .bind(entry.duration_ms)
    .bind(&entry.request_id)
    .bind(entry.bytes_in)
    .bind(entry.bytes_out)
    .bind(entry.error_code.as_deref())
    .fetch_one(pool)
    .await?;

    Ok(row.try_get("id")?)
}

/// Write the whole predicate — clause *and* value, in one place.
///
/// This exists instead of a hand-numbered `$1, $2, …` string for the reason it does in
/// `omnion-media`: a placeholder and the value beside it are written in the same statement, so
/// a filter added later cannot leave a numbering that reads the wrong column. The count and
/// the page both call this, which is what makes them unable to disagree about what was
/// matched. Nothing from the caller is interpolated — the status class is the one exception
/// and it becomes a number derived from a closed list, never the string itself.
fn push_request_filters(
    builder: &mut QueryBuilder<'_, Postgres>,
    organization_id: Uuid,
    query: &RequestLogQuery,
) {
    builder.push("organization_id = ");
    builder.push_bind(organization_id);

    if let Some(key_id) = query.api_key_id {
        builder.push(" and api_key_id = ");
        builder.push_bind(key_id);
    }
    if let Some(key_prefix) = query.key_prefix.as_deref() {
        builder.push(" and api_key_id in (select id from api_keys where prefix = ");
        builder.push_bind(key_prefix.to_owned());
        builder.push(")");
    }
    if let Some(status) = query.status {
        builder.push(" and status = ");
        builder.push_bind(status);
    }
    if let Some(class) = query.status_class.as_deref() {
        // Validated in `normalized` against a closed list, so this match is total.
        let low: i16 = match class {
            "1xx" => 100,
            "2xx" => 200,
            "3xx" => 300,
            "4xx" => 400,
            _ => 500,
        };
        builder.push(format!(" and status >= {low} and status < {}", low + 100));
    }
    if let Some(prefix) = query.path_prefix.as_deref() {
        // Escaped *before* it becomes a pattern, so a `%` a caller typed is a literal
        // character rather than "every path". The `escape` clause is what makes the
        // backslashes mean something to the server.
        builder.push(" and path like ");
        builder.push_bind(format!("{}%", RequestLogQuery::escaped_path_prefix(prefix)));
        builder.push(r" escape '\'");
    }
    if let Some(method) = query.method.as_deref() {
        builder.push(" and method = ");
        builder.push_bind(method.to_owned());
    }
    if let Some(since) = query.since {
        builder.push(" and created_at >= ");
        builder.push_bind(since);
    }
    if let Some(until) = query.until {
        builder.push(" and created_at < ");
        builder.push_bind(until);
    }
    if let Some(minimum) = query.min_duration_ms {
        builder.push(" and duration_ms >= ");
        builder.push_bind(minimum);
    }
}

/// Read a page of the request log.
pub async fn list_requests(
    pool: &PgPool,
    organization_id: Uuid,
    query: &RequestLogQuery,
) -> Result<RequestLogPage> {
    let query = query.clone().normalized()?;

    // Two statements, not one windowed query: the count and the page read the same predicate
    // in the same snapshot, and a `count(*) over ()` would force the whole table through the
    // window buffer just to render "showing 50 of 812".
    let mut counter = QueryBuilder::<Postgres>::new("select count(*) from api_request_logs where ");
    push_request_filters(&mut counter, organization_id, &query);
    let total: i64 = counter.build_query_scalar().fetch_one(pool).await?;

    let mut page = QueryBuilder::<Postgres>::new(
        "select id, organization_id, api_key_id, actor_user_id, method, path, status, duration_ms, \
         request_id, bytes_in, bytes_out, error_code, created_at from api_request_logs where ",
    );
    push_request_filters(&mut page, organization_id, &query);
    page.push(" order by created_at desc, id desc limit ");
    page.push_bind(query.limit);
    page.push(" offset ");
    page.push_bind(query.offset);
    let items: Vec<RequestLog> = page.build_query_as().fetch_all(pool).await?;

    Ok(RequestLogPage {
        has_more: total > query.offset + items.len() as i64,
        items,
        total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cidr_allowlist_is_checked_for_shape_before_it_is_stored() {
        // The four forms an allowlist actually takes.
        for good in ["10.0.0.0/8", "192.168.1.1/32", "2001:db8::/32", "0.0.0.0/0"] {
            assert!(looks_like_cidr(good), "{good} should be accepted");
            let stored = validate_ip_allowlist(&Some(vec![good.to_owned()])).unwrap();
            assert_eq!(stored, Some(vec![good.to_owned()]));
        }
    }

    #[test]
    fn a_mistyped_allowlist_entry_is_refused_by_name() {
        for bad in [
            "10.0.0.0",       // no prefix length
            "10.0.0.0/",      // empty length
            "10.0.0.0/33",    // IPv4 length out of range
            "2001:db8::/129", // IPv6 length out of range
            "not-an-address/8",
            "10.0.0.0/abc",
        ] {
            assert!(!looks_like_cidr(bad), "{bad} should be refused");
            assert!(matches!(
                validate_ip_allowlist(&Some(vec![bad.to_owned()])),
                Err(DeveloperError::InvalidCidr(_))
            ));
        }
    }

    #[test]
    fn an_absent_or_empty_allowlist_means_any_source_rather_than_nothing() {
        // Omitting the field is how a UI says "no restriction", so both spellings of it store
        // the same thing: null, which the edge reads as "allow".
        assert_eq!(validate_ip_allowlist(&None).unwrap(), None);
        assert_eq!(validate_ip_allowlist(&Some(vec![])).unwrap(), None);
        // A list of blanks is the same request, not an allowlist that matches no address.
        assert_eq!(
            validate_ip_allowlist(&Some(vec!["  ".to_owned()])).unwrap(),
            None
        );
    }

    #[test]
    fn an_allowlist_keeps_its_order_and_drops_only_blank_entries() {
        let stored = validate_ip_allowlist(&Some(vec![
            "10.0.0.0/8".to_owned(),
            "  ".to_owned(),
            " 192.168.0.0/16 ".to_owned(),
        ]))
        .unwrap();
        assert_eq!(
            stored,
            Some(vec!["10.0.0.0/8".to_owned(), "192.168.0.0/16".to_owned()])
        );
    }

    #[test]
    fn a_logged_path_never_carries_a_query_string() {
        // The reason `log_request` strips rather than trusting its caller: `?access_token=` is a
        // bearer in a URL, this table is exported to CSV, and an export is the leak.
        assert_eq!(
            path_without_query("/api/v1/media?access_token=super-secret-value"),
            "/api/v1/media"
        );
        assert_eq!(
            path_without_query("/api/v1/developer/oauth/token?client_secret=abc&grant_type=x"),
            "/api/v1/developer/oauth/token"
        );
        // An operator filtering by e-mail would otherwise write every address into the export.
        assert_eq!(
            path_without_query("/api/v1/users?email=furkan@example.com"),
            "/api/v1/users"
        );
    }

    #[test]
    fn a_path_with_nothing_to_strip_is_returned_unchanged() {
        // A strip that also mangles ordinary paths would make the column useless, and a useless
        // column gets worked around by storing the raw path somewhere else.
        assert_eq!(path_without_query("/api/v1/me"), "/api/v1/me");
        assert_eq!(path_without_query("/"), "/");
        assert_eq!(path_without_query(""), "");
        // Only the FIRST `?` ends the path; a later one is part of the query, not the path.
        assert_eq!(path_without_query("/a?b?c=1"), "/a");
        // A fragment never reaches the server, so a path carrying one is constructed elsewhere —
        // but it must not be a way to smuggle the query back in.
        assert_eq!(path_without_query("/api/v1/me?x=1#frag"), "/api/v1/me");
        assert_eq!(path_without_query("#frag"), "");
        // A bare `?` is a query with nothing in it: the path is what precedes it.
        assert_eq!(path_without_query("/api/v1/me?"), "/api/v1/me");
    }
}
