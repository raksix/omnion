//! The store of record for inbound intake declarations and their rejection log (REQ-127, slice 4).
//!
//! [`crate::intake`] decides; this file remembers. The split is the same one `retry_store.rs`
//! and `breaker_store.rs` draw, and it exists for one reason: **a guard whose declarations live
//! in the handler is a guard an operator cannot audit.** Every guarantee the request asks for is
//! a property of what is written down here:
//!
//! * *"`0162` gives the tables; the replay set does not exist in them."* [`remember_signature`]
//!   is the replay defence and it is a **row**, not an in-memory set: a process that restarts
//!   between two deliveries of the same signed request would otherwise accept the second one.
//!   The signature id is stored with the endpoint and the moment it was seen, and the guard is
//!   asked about a window rather than about a set that grows forever.
//! * *"A rejected request leaves a row naming the reason."* [`record_rejection`] is the only
//!   writer, and [`list_rejections`] is what the screen reads — so the operator tunes the
//!   tolerance window against evidence rather than against a guess.
//! * *"No rejected payload is echoed anywhere."* [`record_rejection`] takes a [`crate::intake::Rejection`]
//!   and no body: the store is structurally incapable of writing a payload, and the one field
//!   that looks like one — `body_bytes` — is a count, written by the caller, never read back as
//!   content.
//!
//! ## Why the signature id is a column and not a hash of the tag
//!
//! The replay key is the `v1,<id>` half of the signature, which is what every major provider
//! sends and what they guarantee to be unique per delivery. Hashing the *tag* instead would also
//! work for a scheme without an id, and it is deliberately **not** done: a bare hex tag is
//! accepted by [`crate::intake::verify`] for hand-rolled integrations, and two different
//! legitimate requests from such an integration can share a tag if they share a body. Storing
//! the tag would then refuse the second of two *different* deliveries, which is a false
//! replay. Where no id is present there is no replay key, and [`crate::intake::evaluate`] says
//! so rather than inventing one.

use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::error::{ReliabilityError, Result};
use crate::intake::{IntakeEndpoint, Rejection};
use crate::vocabulary::MAX_PAGE;

#[derive(Debug, sqlx::FromRow)]
struct EndpointRow {
    id: Uuid,
    path: String,
    name: String,
    hmac_scheme: String,
    signature_header: String,
    timestamp_header: Option<String>,
    tolerance_seconds: i32,
    secret_id: Option<Uuid>,
    max_payload_bytes: i32,
    sanitize_profile: String,
    enabled: bool,
    created_at: OffsetDateTime,
}

impl From<EndpointRow> for IntakeEndpoint {
    fn from(row: EndpointRow) -> Self {
        Self {
            path: row.path,
            name: row.name,
            hmac_scheme: row.hmac_scheme,
            signature_header: row.signature_header,
            timestamp_header: row.timestamp_header,
            tolerance_seconds: row.tolerance_seconds,
            secret_id: row.secret_id,
            max_payload_bytes: row.max_payload_bytes,
            sanitize_profile: row.sanitize_profile,
            enabled: row.enabled,
        }
    }
}

/// One declaration, as the panel reads it: the guard's configuration plus the two numbers the
/// list shows beside it.
///
/// `rejection_count` and `last_rejection_at` are `0`/`None` on a single-row read ([`load`],
/// [`find_by_path`]) because those do not aggregate without a query that is not worth one. That
/// is stated here rather than left for the caller to discover: a screen showing "0 refusals" next
/// to an endpoint that has refused four is displaying a number nobody computed.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StoredEndpoint {
    /// Flattened so the panel's payload for one endpoint and the list's are the same object.
    #[serde(flatten)]
    pub endpoint: IntakeEndpoint,
    pub id: Uuid,
    pub created_at: OffsetDateTime,
    /// Refusals against this declaration inside the retention window, for the list's column.
    pub rejection_count: i64,
    /// The most recent refusal, so a list sorted by time needs no second read.
    pub last_rejection_at: Option<OffsetDateTime>,
}

/// One rejection row, as the screen's log reads it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RejectionRow {
    pub id: i64,
    pub endpoint_id: Option<Uuid>,
    /// The declared path, resolved on read so a renamed endpoint does not orphan its log.
    pub path: Option<String>,
    pub reason: String,
    /// Text, not `inet`: the log is read by a person and a person needs a string; the column is
    /// still `inet`, so a value that is not an address could never have been written.
    pub source_ip: Option<String>,
    pub request_id: Option<Uuid>,
    pub body_bytes: i32,
    pub created_at: OffsetDateTime,
}

#[derive(Debug, sqlx::FromRow)]
struct RejectionDbRow {
    id: i64,
    endpoint_id: Option<Uuid>,
    path: Option<String>,
    reason: String,
    source_ip: Option<String>,
    request_id: Option<Uuid>,
    body_bytes: i32,
    created_at: OffsetDateTime,
}

impl From<RejectionDbRow> for RejectionRow {
    fn from(row: RejectionDbRow) -> Self {
        Self {
            id: row.id,
            endpoint_id: row.endpoint_id,
            path: row.path,
            reason: row.reason,
            source_ip: row.source_ip,
            request_id: row.request_id,
            body_bytes: row.body_bytes,
            created_at: row.created_at,
        }
    }
}

/// The list row: the declaration plus the two rollup columns, in ONE query.
#[derive(Debug, sqlx::FromRow)]
struct ListRow {
    #[sqlx(flatten)]
    row: EndpointRow,
    rejection_count: i64,
    last_rejection_at: Option<OffsetDateTime>,
}

const ENDPOINT_COLUMNS: &str = "id, path, name, hmac_scheme, signature_header, timestamp_header, \
     tolerance_seconds, secret_id, max_payload_bytes, sanitize_profile, enabled, created_at";

impl StoredEndpoint {
    /// Build the read model from a row, with the rollup the caller actually computed.
    ///
    /// **One destructuring constructor rather than a `From` plus field-by-field literals at
    /// three call sites.** The first draft had `StoredEndpoint::from(row)` and read `r.id` in
    /// the same literal that consumed `r` — two errors, one per symptom: `r` moved by `into()`
    /// and used after, and — the one worth remembering — a `From` impl written on the wrong
    /// `FromRow` type, so the compiler resolved `RejectionRow::from` to the blanket
    /// `From<RejectionRow> for RejectionRow` and reported a *type mismatch* rather than a
    /// missing impl. A destination type that a blanket impl already covers is a shape that
    /// silently compiles into the wrong conversion.
    fn from_row(row: EndpointRow, rejection_count: i64, last_rejection_at: Option<OffsetDateTime>) -> Self {
        let EndpointRow {
            id,
            path,
            name,
            hmac_scheme,
            signature_header,
            timestamp_header,
            tolerance_seconds,
            secret_id,
            max_payload_bytes,
            sanitize_profile,
            enabled,
            created_at,
        } = row;
        Self {
            id,
            created_at,
            endpoint: IntakeEndpoint {
                path,
                name,
                hmac_scheme,
                signature_header,
                timestamp_header,
                tolerance_seconds,
                secret_id,
                max_payload_bytes,
                sanitize_profile,
                enabled,
            },
            rejection_count,
            last_rejection_at,
        }
    }
}

/// Every declaration, ordered by path so the list is stable between reads.
pub async fn list(pool: &PgPool) -> Result<Vec<StoredEndpoint>> {
    let sql = format!(
        "select {ENDPOINT_COLUMNS},
                (select count(*) from intake_rejections r where r.endpoint_id = intake_endpoints.id)::bigint
                    as rejection_count,
                (select max(r.created_at) from intake_rejections r where r.endpoint_id = intake_endpoints.id)
                    as last_rejection_at
         from intake_endpoints order by path asc"
    );
    // ONE statement. The rollup is a correlated sub-select rather than a second read per row:
    // a list an operator opens while an integration is failing is exactly the list that must not
    // be N+1, and `count` over an indexed `(endpoint_id, created_at desc)` is a range scan
    // per row rather than a table scan.
    let rows = sqlx::query_as::<_, ListRow>(&sql)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            StoredEndpoint::from_row(row.row, row.rejection_count, row.last_rejection_at)
        })
        .collect())
}



pub async fn load(pool: &PgPool, id: Uuid) -> Result<Option<StoredEndpoint>> {
    let sql = format!("select {ENDPOINT_COLUMNS} from intake_endpoints where id = $1");
    let row = sqlx::query_as::<_, EndpointRow>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|row| StoredEndpoint::from_row(row, 0, None)))
}

/// The declaration guarding `path`, or `None` when nothing is declared there.
///
/// Exact match, deliberately. A prefix match would let a declaration for
/// `/api/v1/hooks/stripe` guard `/api/v1/hooks/stripe-admin` as a side effect, and a guard that
/// guards more than it says is a guard an operator cannot reason about. The declared path is
/// exact because that is what the screen shows and what the caller must type.
pub async fn find_by_path(pool: &PgPool, path: &str) -> Result<Option<StoredEndpoint>> {
    let sql = format!("select {ENDPOINT_COLUMNS} from intake_endpoints where path = $1");
    let row = sqlx::query_as::<_, EndpointRow>(&sql)
        .bind(path)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|row| StoredEndpoint::from_row(row, 0, None)))
}

/// Create a declaration.
///
/// The route's `validate()` has already run before this is called, and it is **not** re-run
/// here: a store that re-validates a domain type it was handed invites the second caller to skip
/// it, and the two would then disagree about what a legal declaration is. The `check`
/// constraints in `0162` remain the last line of defence underneath both.
pub async fn insert(pool: &PgPool, endpoint: &IntakeEndpoint) -> Result<StoredEndpoint> {
    let row: (Uuid,) = sqlx::query_as(
        "insert into intake_endpoints
            (path, name, hmac_scheme, signature_header, timestamp_header, tolerance_seconds,
             secret_id, max_payload_bytes, sanitize_profile, enabled)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         returning id",
    )
    .bind(&endpoint.path)
    .bind(&endpoint.name)
    .bind(&endpoint.hmac_scheme)
    .bind(&endpoint.signature_header)
    .bind(&endpoint.timestamp_header)
    .bind(endpoint.tolerance_seconds)
    .bind(endpoint.secret_id)
    .bind(endpoint.max_payload_bytes)
    .bind(&endpoint.sanitize_profile)
    .bind(endpoint.enabled)
    .fetch_one(pool)
    .await?;
    load(pool, row.0)
        .await?
        .ok_or(ReliabilityError::NotFound)
}

/// Edit a declaration.
///
/// `path` is editable on purpose — a provider moves its endpoint and the operator would otherwise
/// have to delete the row and lose its rejection history. The `unique` constraint is what
/// refuses a collision, and it is caught and named rather than surfacing as a `23505`.
pub async fn update(pool: &PgPool, id: Uuid, endpoint: &IntakeEndpoint) -> Result<StoredEndpoint> {
    let affected = sqlx::query(
        "update intake_endpoints set
            path = $2, name = $3, hmac_scheme = $4, signature_header = $5, timestamp_header = $6,
            tolerance_seconds = $7, secret_id = $8, max_payload_bytes = $9,
            sanitize_profile = $10, enabled = $11
         where id = $1",
    )
    .bind(id)
    .bind(&endpoint.path)
    .bind(&endpoint.name)
    .bind(&endpoint.hmac_scheme)
    .bind(&endpoint.signature_header)
    .bind(&endpoint.timestamp_header)
    .bind(endpoint.tolerance_seconds)
    .bind(endpoint.secret_id)
    .bind(endpoint.max_payload_bytes)
    .bind(&endpoint.sanitize_profile)
    .bind(endpoint.enabled)
    .execute(pool)
    .await?
    .rows_affected();
    if affected == 0 {
        return Err(ReliabilityError::NotFound);
    }
    load(pool, id).await?
        .ok_or(ReliabilityError::NotFound)
}

/// Remove a declaration. Its rejection rows go with it, by `on delete cascade`.
///
/// A rejected inbound request is evidence, and evidence that is deleted with the declaration is
/// evidence an operator loses at the worst moment — so the route asks for a reason and the
/// reason travels in the audit row. The cascade is the migration's decision and it is stated
/// here rather than discovered: keeping the rows would need a nullable endpoint id to stay
/// meaningful once the path is gone.
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<bool> {
    let removed = sqlx::query("delete from intake_endpoints where id = $1")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    if removed == 0 {
        return Ok(false);
    }
    sqlx::query("delete from intake_seen_signatures where endpoint_id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(true)
}

/// Write one rejection. The only writer of `intake_rejections`.
///
/// `record: false` verdicts never reach here: a disabled endpoint is not a rejection of a
/// request, it is a path nobody is serving, and a log full of those trains an operator to
/// ignore the log.
pub async fn record_rejection(
    pool: &PgPool,
    endpoint_id: Option<Uuid>,
    rejection: &Rejection,
) -> Result<()> {
    // `source_ip` is `inet` in `0162` and the domain type is `IpAddr`, so the statement casts
    // rather than binding. **This was the slice's one real bug and the walk found it on its
    // first run.** sqlx binds an `IpAddr` as TEXT, and PostgreSQL refuses the assignment with
    // `42804 column "source_ip" is of type inet but expression is of type text` — so the insert
    // failed for every rejection, and the caller logged the failure at `warn` and answered the
    // refusal anyway. Every acceptance criterion that says "and a rejection row" was failing
    // while the STATUS assertions all passed, because the guard's answer never depended on the
    // log being written.
    //
    // The lesson is the ordering of that failure: the thing that was supposed to be evidence was
    // best-effort, and best-effort evidence is not evidence. A walk that asserted only the
    // status would have called this slice done.
    sqlx::query(
        "insert into intake_rejections (endpoint_id, reason, source_ip, request_id, body_bytes)
         values ($1, $2, $3::inet, $4, $5)",
    )
    .bind(endpoint_id)
    .bind(&rejection.reason)
    .bind(rejection.source_ip.map(|ip| ip.to_string()))
    .bind(rejection.request_id)
    .bind(rejection.body_bytes as i32)
    .execute(pool)
    .await?;
    Ok(())
}

/// The rejection log, newest first, optionally narrowed to one endpoint.
pub async fn list_rejections(
    pool: &PgPool,
    endpoint_id: Option<Uuid>,
    reason: Option<&str>,
    limit: usize,
) -> Result<Vec<RejectionRow>> {
    let reason = reason.filter(|r| !r.trim().is_empty());
    let rows = sqlx::query_as::<_, RejectionDbRow>(
        "select r.id, r.endpoint_id, e.path, r.reason, host(r.source_ip) as source_ip,
                r.request_id, r.body_bytes, r.created_at
         from intake_rejections r
         left join intake_endpoints e on e.id = r.endpoint_id
         where ($1::uuid is null or r.endpoint_id = $1)
           and ($2::text is null or r.reason = $2)
         order by r.created_at desc, r.id desc
         limit $3",
    )
    .bind(endpoint_id)
    .bind(reason)
    .bind(limit.clamp(1, MAX_PAGE) as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(RejectionRow::from).collect())
}

/// Refusal counts per reason, for the screen's chips.
pub async fn reason_counts(pool: &PgPool) -> Result<Vec<(String, i64)>> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "select reason, count(*)::bigint as count from intake_rejections
         group by reason order by count desc, reason asc",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Claim a signature id for this endpoint, or report that it was already claimed.
///
/// **This is the replay defence, and it is a single conditional insert.** The read-then-write
/// shape is the one that fails here: two deliveries of the same signed request arrive together,
/// both read "not seen", both proceed, and the guard that exists to prevent exactly that lets
/// both through. The `unique` index plus `on conflict do nothing` makes the database arbitrate,
/// and `rows_affected() == 0` is the answer — so the caller learns the truth from the write
/// rather than from a read that may already be stale.
///
/// The row is inserted only **after** the tag verifies. A signature id that is recorded before
/// authentication is a denial-of-service an attacker gets for free: send garbage with somebody
/// else's id and their next legitimate delivery is refused as a replay.
pub async fn remember_signature(
    pool: &PgPool,
    endpoint_id: Uuid,
    signature_id: &str,
    tolerance_seconds: i32,
) -> Result<bool> {
    let affected = sqlx::query(
        "insert into intake_seen_signatures (endpoint_id, signature_id, seen_at, expires_at)
         values ($1, $2, now(), now() + make_interval(secs => $3))
         on conflict (endpoint_id, signature_id) do nothing",
    )
    .bind(endpoint_id)
    .bind(signature_id)
    .bind(f64::from(tolerance_seconds))
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

/// Every signature id still inside its window for one endpoint, as the guard's `seen_ids` set.
///
/// Read once per request rather than per signature: a request carries one signature, so a
/// per-signature query would be a query whose result is used once and thrown away, and the
/// set-shaped call [`crate::intake::evaluate`] takes is what lets the *decision function* stay
/// pure and testable without a database.
pub async fn seen_signature_ids(
    pool: &PgPool,
    endpoint_id: Uuid,
    now: OffsetDateTime,
) -> Result<std::collections::BTreeSet<String>> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "select signature_id from intake_seen_signatures
         where endpoint_id = $1 and expires_at > $2",
    )
    .bind(endpoint_id)
    .bind(now)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Drop the ids whose window has passed, and the rejections past the retention window.
///
/// Both are the same statement shape for the same reason: the replay table is bounded by
/// construction only if something actually removes rows from it, and a "seen ids" table nobody
/// prunes is a table that eventually refuses a legitimate delivery whose id happens to repeat.
pub async fn prune(
    pool: &PgPool,
    signature_days: i64,
    rejection_days: i64,
) -> Result<(u64, u64)> {
    let signatures = sqlx::query("delete from intake_seen_signatures where expires_at <= now()")
        .execute(pool)
        .await?
        .rows_affected();
    let cutoff = OffsetDateTime::now_utc() - Duration::days(rejection_days);
    let rejections = sqlx::query("delete from intake_rejections where created_at < $1")
        .bind(cutoff)
        .execute(pool)
        .await?
        .rows_affected();
    Ok((signatures, rejections))
}

/// Why a save was refused, when the answer is a constraint rather than a validation.
///
/// sqlx surfaces a `23505` as a database error with the constraint name buried in its message, so
/// the route would answer a name collision with `500 internal` and a sentence about a unique
/// violation. This turns the two constraint failures an operator can cause into the message the
/// form shows.
///
/// **Takes the crate's error, not a `sqlx::Error`.** `insert` wraps the database failure in
/// `ReliabilityError::Database` on the way out, so a signature asking for the raw error is
/// unsatisfiable at the only call site that has one — which is how a `&sqlx::Error` parameter
/// becomes a function nothing can call. It reaches the SQLSTATE through
/// [`ReliabilityError::database_error`] instead of taking the whole error apart again.
pub fn explain_constraint(error: &ReliabilityError, constraint: &str) -> Option<String> {
    let db = error.database_error()?;
    let sqlx::Error::Database(db) = db else {
        return None;
    };
    if db.code().as_deref() != Some("23505") {
        return None;
    }
    let message = db.message().to_owned();
    if !message.contains(constraint) {
        return None;
    }
    Some(format!(
        "another intake endpoint already uses this {constraint} — pick a different one"
    ))
}
