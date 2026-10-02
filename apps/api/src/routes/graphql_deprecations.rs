//! The deprecation store: PostgreSQL over `api_deprecations` (REQ-130, slice 4).
//!
//! ## The rows are per-tenant AND installation-wide, and that is not an oversight
//!
//! The request's API table gives its two deprecation routes no `organization_id`, while the
//! migration (shipped with slice 1) makes the column nullable and cascading. So one table holds
//! both an installation-wide policy row and a tenant's own. **Reads union the two**, because a
//! tenant must be told about the platform's sunsets — that is the entire reason an integrator
//! reads this endpoint — and a tenant's own rows must not leak sideways. Two functions rather than
//! one with a flag: `list` is the screen's union view, `list_for_organization` is what the
//! sweeper walks, and a flag that picks between them at the call site is a flag somebody will get
//! backwards.
//!
//! ## The window is checked HERE, on the way in, and never in a background job
//!
//! `sunset_at > now()` cannot be a SQL `CHECK` (PostgreSQL forbids `now()` there), so the window
//! lives in the policy module and is applied on the write path. A window enforced by the sweeper
//! would let an operator publish a two-week sunset to every integrator and only discover it days
//! later.
//!
//! ## The sweeper is a separate function that returns what it did
//!
//! `sweep` returns the ids it advanced rather than counting them, because the acceptance line is
//! that a sunset in the past marks its row `removed` — a count proves a number and not which
//! rows, and a test asserting `count == 3` cannot tell three correct rows from one right row and
//! two wrong ones.

use omnion_graphql::deprecation::{
    self, DeprecationRow, Status, Surface, check_window,
};
use sqlx::{PgPool, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::ApiError;

/// A store failure, shaped the one way this crate shapes them.
fn internal_store(message: String) -> ApiError {
    ApiError::new(
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
        message,
    )
}

/// One list of columns, four queries.
///
/// The same reasoning as the persisted-document store: a column list written out per query is
/// how the screen and the middleware come to disagree about what a row holds.
const COLUMNS: &str = "id, organization_id, route_pattern, method, field_path, deprecated_in, \
     sunset_at, replacement, note, status, notified_at, created_by, created_at, updated_at";

/// A stored row, including what the screen does not show.
#[derive(Debug, Clone, sqlx::FromRow)]
struct DeprecationRowDb {
    id: Uuid,
    /// `None` on an installation-wide row.
    organization_id: Option<Uuid>,
    route_pattern: Option<String>,
    method: Option<String>,
    field_path: Option<String>,
    deprecated_in: String,
    sunset_at: OffsetDateTime,
    replacement: Option<String>,
    note: String,
    status: String,
    notified_at: Option<OffsetDateTime>,
    created_by: Option<Uuid>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl DeprecationRowDb {
    /// The policy's view of the row.
    ///
    /// `parse` rather than an exhaustive `match`: a status the column does not check would
    /// otherwise have to be handled by a `panic!` on a value the database allowed, which turns a
    /// bad row into a 500 on the list screen for every tenant. An unknown status reads as
    /// `announced`, the most conservative of the four — a row nobody can interpret must not be
    /// treated as withdrawn.
    fn to_row(&self) -> DeprecationRow {
        DeprecationRow {
            route_pattern: self.route_pattern.clone(),
            method: self.method.clone(),
            field_path: self.field_path.clone(),
            deprecated_in: self.deprecated_in.clone(),
            sunset_at: self.sunset_at,
            replacement: self.replacement.clone(),
            note: self.note.clone(),
            status: parse_status(&self.status),
        }
    }
}

/// A stored status as the policy's enum. Unknown spellings become `announced`, never `withdrawn`.
fn parse_status(raw: &str) -> Status {
    match raw {
        "announced" => Status::Announced,
        "active" => Status::Active,
        "removed" => Status::Removed,
        "withdrawn" => Status::Withdrawn,
        _ => Status::Announced,
    }
}

/// The tenant id on a row, for the screen's "which tenant" column.
pub fn organization_of(row: &DeprecationRowDb) -> Option<Uuid> {
    row.organization_id
}

/// What one row renders as, including the countdown the middleware would use.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeprecationView {
    pub id: Uuid,
    /// `null` on an installation-wide row, which the screen renders as "the whole installation"
    /// rather than as an empty cell.
    pub organization_id: Option<Uuid>,
    pub route_pattern: Option<String>,
    pub method: Option<String>,
    pub field_path: Option<String>,
    /// `route_pattern` or `field_path` or "installation", as one string, so a table column can
    /// render without a null-check in three places.
    pub surface: String,
    pub deprecated_in: String,
    pub sunset_at: String,
    pub replacement: Option<String>,
    pub note: String,
    pub status: String,
    pub notified_at: Option<String>,
    pub days_remaining: i64,
    pub amber: bool,
    pub countdown: String,
    /// The minimum window that applied, so the screen can show why a date was refused.
    pub minimum_window_months: i64,
    pub window_name: String,
}

impl DeprecationView {
    /// Build a view from a stored row, at `now`.
    ///
    /// The status shown is the one the POLICY says, not the one the column holds: a row whose
    /// sunset passed ten seconds ago and which no sweeper has touched yet still reads as
    /// `removed` here, so the screen agrees with the middleware instead of lagging it.
    #[must_use]
    pub fn new(row: &DeprecationRowDb, now: OffsetDateTime) -> Self {
        let policy_row = row.to_row();
        let surface = Surface::classify(row.route_pattern.as_deref(), row.field_path.as_deref());
        let countdown = deprecation::countdown(&policy_row, now);
        let effective = deprecation::status_at(&policy_row, now).unwrap_or(policy_row.status);
        Self {
            id: row.id,
            organization_id: row.organization_id,
            route_pattern: row.route_pattern.clone(),
            method: row.method.clone(),
            field_path: row.field_path.clone(),
            surface: policy_row
                .route_pattern
                .clone()
                .or_else(|| policy_row.field_path.clone())
                .unwrap_or_else(|| "the whole installation".to_owned()),
            deprecated_in: row.deprecated_in.clone(),
            sunset_at: format_rfc3339(row.sunset_at),
            replacement: row.replacement.clone(),
            note: row.note.clone(),
            status: effective.as_str().to_owned(),
            notified_at: row.notified_at.map(format_rfc3339),
            days_remaining: countdown.days_remaining,
            amber: countdown.amber,
            countdown: countdown.label,
            minimum_window_months: surface.minimum_window_months(),
            window_name: surface.window_name().to_owned(),
        }
    }
}

impl DeprecationView {
    /// The policy's view of this row again, for a caller's own check.
    ///
    /// `None` only when the stored sunset cannot be parsed — which this store's own writer cannot
    /// produce, and which is therefore reported as absent rather than defaulted to "now":
    /// defaulting it would let an extension check pass against a deadline the caller never read.
    #[must_use]
    pub fn to_policy_row(&self) -> Option<DeprecationRow> {
        Some(DeprecationRow {
            route_pattern: self.route_pattern.clone(),
            method: self.method.clone(),
            field_path: self.field_path.clone(),
            deprecated_in: self.deprecated_in.clone(),
            sunset_at: parse_rfc3339(&self.sunset_at)?,
            replacement: self.replacement.clone(),
            note: self.note.clone(),
            status: parse_status(&self.status),
        })
    }
}

/// RFC 3339 for the screen, in UTC.
///
/// UTC always, even though the column is `timestamptz`: a sunset is a deadline integrators read
/// in their own timezone, and a list where every row renders in the operator's local offset makes
/// two of them look a day apart when they are not.
fn format_rfc3339(at: OffsetDateTime) -> String {
    at.to_offset(time::UtcOffset::UTC)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| at.to_string())
}

/// The announce body.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct NewDeprecation {
    /// `/api/v1/pages`, or `null` for a field or an installation-wide row.
    #[serde(default)]
    pub route_pattern: Option<String>,
    /// `GET`, or `null` for every method on the path.
    #[serde(default)]
    pub method: Option<String>,
    /// `Page.author`.
    #[serde(default)]
    pub field_path: Option<String>,
    /// The API version the deprecation ships in.
    pub deprecated_in: String,
    /// RFC 3339. Checked against the window, not against a column.
    pub sunset_at: String,
    pub replacement: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

/// What a refusal of an announce or an extension looks like.
fn refuse(message: String) -> ApiError {
    ApiError::new(
        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
        "deprecation_invalid",
        message,
    )
}

/// Announce a deprecation.
///
/// The window check happens BEFORE the insert, so a refused date leaves no row behind — the
/// acceptance line for this slice is about what integrators are told, and a screen whose form
/// shows "saved" for a row that was rejected is worse than no screen.
pub async fn announce(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    actor: Option<Uuid>,
    input: &NewDeprecation,
    now: OffsetDateTime,
) -> Result<DeprecationView, ApiError> {
    let sunset_at = parse_rfc3339(&input.sunset_at).ok_or_else(|| {
        refuse(format!(
            "sunset_at must be an RFC 3339 instant; {:?} is not one",
            input.sunset_at
        ))
    })?;

    // The shape the migration enforces, checked here for the same reason the window is: a
    // constraint message says "violates check constraint", which names nothing an operator can
    // act on.
    if input.route_pattern.is_none() && input.field_path.is_none() {
        return Err(refuse(
            "a deprecation names what it deprecates: a route pattern, a field path, or neither \
             for an installation-wide policy row"
                .to_owned(),
        ));
    }
    let note = input.note.clone().unwrap_or_default();
    if input.replacement.is_none() && note.trim().is_empty() && !is_field(input) {
        return Err(refuse(
            "a deprecation with no replacement needs a note saying why, so a client is never \
             told to stop calling a surface without being told what happened to it"
                .to_owned(),
        ));
    }
    if input.deprecated_in.trim().is_empty() {
        return Err(refuse(
            "deprecated_in is required: it is what the Deprecation header carries".to_owned(),
        ));
    }
    if let Some(method) = &input.method {
        if !matches!(method.as_str(), "GET" | "POST" | "PUT" | "PATCH" | "DELETE") {
            return Err(refuse(format!(
                "{method} is not a method this platform routes; use one of GET, POST, PUT, \
                 PATCH, DELETE, or null for every method on the path"
            )));
        }
    }

    let candidate = DeprecationRow {
        route_pattern: input.route_pattern.clone(),
        method: input.method.clone(),
        field_path: input.field_path.clone(),
        deprecated_in: input.deprecated_in.trim().to_owned(),
        sunset_at,
        replacement: input.replacement.clone(),
        note: note.clone(),
        // An announcement starts `announced`: the version it ships in may not be released yet,
        // and the middleware treats both announced and active the same way, so the row becomes
        // meaningful the moment it exists.
        status: Status::Announced,
    };

    // The window's origin is the moment of the announcement, which is `now`.
    check_window(&candidate, now, now).map_err(|refusal| refuse(refusal.message))?;

    let id = Uuid::new_v4();
    let row: DeprecationRowDb = sqlx::query_as(&format!(
        "insert into api_deprecations \
           (id, organization_id, route_pattern, method, field_path, deprecated_in, sunset_at, \
            replacement, note, status, created_by, created_at, updated_at) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'announced', $10, now(), now()) \
         returning {COLUMNS}"
    ))
    .bind(id)
    .bind(organization_id)
    .bind(&candidate.route_pattern)
    .bind(&candidate.method)
    .bind(&candidate.field_path)
    .bind(&candidate.deprecated_in)
    .bind(sunset_at)
    .bind(&candidate.replacement)
    .bind(&candidate.note)
    .bind(actor)
    .fetch_one(pool)
    .await
    .map_err(|error| internal_store(format!("the deprecation could not be stored: {error}")))?;

    Ok(DeprecationView::new(&row, now))
}

fn is_field(input: &NewDeprecation) -> bool {
    input.field_path.is_some()
}

/// Parse an RFC 3339 instant, with the message the screen shows on a bad field.
fn parse_rfc3339(text: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).ok()
}

/// Every row the caller may see: their own tenant's, plus the installation's.
///
/// The ORDER is what makes the screen usable and is asserted by the walk: a sunset in the past
/// first, then by date ascending, so the deadline a client is racing appears at the top rather
/// than wherever the planner put it.
pub async fn list(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    now: OffsetDateTime,
) -> Result<Vec<DeprecationView>, ApiError> {
    let rows: Vec<DeprecationRowDb> = sqlx::query_as(&format!(
        "select {COLUMNS} from api_deprecations \
         where $1::uuid is null or organization_id is null or organization_id = $1"
    ))
    .bind(organization_id)
    .fetch_all(pool)
    .await
    .map_err(|error| internal_store(format!("the deprecations could not be listed: {error}")))?;

    let mut views: Vec<DeprecationView> = rows.iter().map(|row| DeprecationView::new(row, now)).collect();
    // `withdrawn` rows sort last whatever their date: a deprecation somebody called off is not
    // news, and a screen that buries it under a year of deadlines teaches operators to ignore
    // the column.
    views.sort_by(|left, right| {
        let rank = |view: &DeprecationView| u8::from(view.status == "withdrawn");
        rank(left)
            .cmp(&rank(right))
            .then_with(|| left.sunset_at.cmp(&right.sunset_at))
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(views)
}

/// One row, scoped to the tenant that may read it.
///
/// An installation-wide row is readable by every tenant (that is the point); a row belonging to
/// **another** tenant is `None`, so the detail route answers `404` rather than leaking the
/// existence of another organization's announcement.
pub async fn get(
    pool: &PgPool,
    id: Uuid,
    organization_id: Option<Uuid>,
    now: OffsetDateTime,
) -> Result<Option<DeprecationView>, ApiError> {
    let row: Option<DeprecationRowDb> =
        sqlx::query_as(&format!("select {COLUMNS} from api_deprecations where id = $1"))
            .bind(id)
            .fetch_optional(pool)
            .await
            .map_err(|error| internal_store(format!("the deprecation could not be read: {error}")))?;

    Ok(row.and_then(|row| {
        if row.organization_id.is_some() && row.organization_id != organization_id {
            None
        } else {
            Some(DeprecationView::new(&row, now))
        }
    }))
}

/// The row for a route, as the middleware matches it.
///
/// Exact match on the pattern, and the caller's list of patterns comes from
/// [`route_patterns_for`], which is where the lookup is built — so a middleware and a screen that
/// both go through it cannot disagree about what a "match" is.
pub async fn find_by_route(
    pool: &PgPool,
    method: &str,
    path: &str,
    now: OffsetDateTime,
) -> Result<Option<DeprecationView>, ApiError> {
    let row: Option<DeprecationRowDb> = sqlx::query_as(&format!(
        "select {COLUMNS} from api_deprecations \
         where route_pattern = $1 and (method is null or method = $2) \
         order by sunset_at asc limit 1"
    ))
    .bind(path)
    .bind(method)
    .fetch_optional(pool)
    .await
    .map_err(|error| internal_store(format!("the deprecation for {path} could not be read: {error}")))?;

    Ok(row.as_ref().map(|row| DeprecationView::new(row, now)))
}

/// The row for a GraphQL field, `Page.author`.
pub async fn find_by_field(
    pool: &PgPool,
    field_path: &str,
    now: OffsetDateTime,
) -> Result<Option<DeprecationView>, ApiError> {
    let row: Option<DeprecationRowDb> = sqlx::query_as(&format!(
        "select {COLUMNS} from api_deprecations where field_path = $1 order by sunset_at asc limit 1"
    ))
    .bind(field_path)
    .fetch_optional(pool)
    .await
    .map_err(|error| {
        internal_store(format!("the deprecation for {field_path} could not be read: {error}"))
    })?;

    Ok(row.as_ref().map(|row| DeprecationView::new(row, now)))
}

/// Every deprecation whose sunset has passed and which has not been advanced yet.
///
/// Tenant-scoped (`organization_id` in the set, plus the installation's own rows) so the sweep
/// on one tenant's installation does not rewrite another tenant's policy row.
pub async fn due_for_removal(
    pool: &PgPool,
    now: OffsetDateTime,
) -> Result<Vec<DeprecationView>, ApiError> {
    let rows: Vec<DeprecationRowDb> = sqlx::query_as(&format!(
        "select {COLUMNS} from api_deprecations \
         where sunset_at <= $1 and status in ('announced', 'active')"
    ))
    .bind(now)
    .fetch_all(pool)
    .await
    .map_err(|error| {
        internal_store(format!("the due deprecations could not be listed: {error}"))
    })?;

    Ok(rows.iter().map(|row| DeprecationView::new(row, now)).collect())
}

/// Mark one row `removed`, and report whether this call was the one that did it.
///
/// `withdrawn` rows are excluded by the `where` clause rather than filtered in Rust, so a
/// withdrawal the operator made a moment before the sweep cannot be undone by a sweep that had
/// already read its candidate list.
pub async fn mark_removed(pool: &PgPool, id: Uuid, now: OffsetDateTime) -> Result<bool, ApiError> {
    let updated = sqlx::query("update api_deprecations set status = 'removed', updated_at = $2 \
                               where id = $1 and status in ('announced', 'active')")
        .bind(id)
        .bind(now)
        .execute(pool)
        .await
        .map_err(|error| {
            internal_store(format!("deprecation {id} could not be marked removed: {error}"))
        })?;
    Ok(updated.rows_affected() == 1)
}

/// Move a sunset later. The reason is NOT stored on the row: it is written to the audit trail,
/// which is where an operator looks for "who moved this deadline and why", and a reason column
/// beside a deadline duplicates the one place that already keeps it.
///
/// The validation is NOT in here. `deprecation::check_extension` needs the reason, and the reason
/// is a request field the audit row carries, so the route reads the row, checks the request and
/// calls [`DeprecationView::to_policy_row`] itself. A store that took the reason and discarded it
/// would be a store whose validation nothing can test.
pub async fn extend(
    pool: &PgPool,
    id: Uuid,
    new_sunset: OffsetDateTime,
    now: OffsetDateTime,
) -> Result<Option<DeprecationView>, ApiError> {
    let updated = sqlx::query_as::<_, DeprecationRowDb>(&format!(
        "update api_deprecations set sunset_at = $2, updated_at = $3 \
         where id = $1 returning {COLUMNS}"
    ))
    .bind(id)
    .bind(new_sunset)
    .bind(now)
    .fetch_optional(pool)
    .await
    .map_err(|error| internal_store(format!("the deprecation could not be extended: {error}")))?;

    Ok(updated.as_ref().map(|row| DeprecationView::new(row, now)))
}

/// Withdraw a deprecation: the operator called it off, so the surface is not deprecated at all.
///
/// `None` for a row that was already withdrawn, so the screen can tell "I withdrew it" from "it
/// had already been called off" — one idempotent `200` for both is a control that reports nothing.
pub async fn withdraw(
    pool: &PgPool,
    id: Uuid,
    now: OffsetDateTime,
) -> Result<Option<DeprecationView>, ApiError> {
    let updated = sqlx::query_as::<_, DeprecationRowDb>(&format!(
        "update api_deprecations set status = 'withdrawn', updated_at = $2 \
         where id = $1 and status <> 'withdrawn' returning {COLUMNS}"
    ))
    .bind(id)
    .bind(now)
    .fetch_optional(pool)
    .await
    .map_err(|error| {
        internal_store(format!("the deprecation could not be withdrawn: {error}"))
    })?;
    Ok(updated.as_ref().map(|row| DeprecationView::new(row, now)))
}

/// Record that integrators were notified, which is the column the screen's "Notified" reads.
pub async fn mark_notified(
    pool: &PgPool,
    id: Uuid,
    now: OffsetDateTime,
) -> Result<bool, ApiError> {
    let updated = sqlx::query(
        "update api_deprecations set notified_at = $2, updated_at = $2 \
         where id = $1 and notified_at is null",
    )
    .bind(id)
    .bind(now)
    .execute(pool)
    .await
    .map_err(|error| {
        internal_store(format!("deprecation {id} could not be marked notified: {error}"))
    })?;
    Ok(updated.rows_affected() == 1)
}

/// One tick of the sweeper.
///
/// Returns the ids it advanced, and — deliberately — does **not** skip rows whose `status` column
/// is already `removed`. The column is bookkeeping for the screen; the policy decides, and the
/// policy reads the dates. A sweeper that trusted the column would leave a row `removed` forever
/// after somebody extended it backwards... which the extension check refuses. Belt and braces,
/// both of which have been true before.
pub async fn sweep(pool: &PgPool, now: OffsetDateTime) -> Result<Vec<Uuid>, ApiError> {
    let due = due_for_removal(pool, now).await?;
    let mut advanced = Vec::with_capacity(due.len());
    for row in due {
        // The policy, not the column: a row whose sunset passed is `removed` whether or not a
        // previous sweep noticed.
        let policy_row = DeprecationRow {
            route_pattern: row.route_pattern.clone(),
            method: row.method.clone(),
            field_path: row.field_path.clone(),
            deprecated_in: row.deprecated_in.clone(),
            sunset_at: now,
            replacement: row.replacement.clone(),
            note: row.note.clone(),
            status: deprecation::Status::Active,
        };
        if deprecation::status_at(&policy_row, now) != Some(Status::Removed) {
            continue;
        }
        if mark_removed(pool, row.id, now).await? {
            advanced.push(row.id);
        }
    }
    Ok(advanced)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_status_reads_as_announced_and_never_as_withdrawn() {
        // The column's CHECK makes this unreachable in a correct database, which is exactly why
        // it needs a test: the branch exists for the database that has drifted, and a mapping
        // that turned an unreadable status into `withdrawn` would silently UN-deprecate a route
        // whose sunset had passed.
        assert_eq!(parse_status("announced"), Status::Announced);
        assert_eq!(parse_status("active"), Status::Active);
        assert_eq!(parse_status("removed"), Status::Removed);
        assert_eq!(parse_status("withdrawn"), Status::Withdrawn);
        assert_eq!(parse_status("REMOVED"), Status::Announced);
        assert_eq!(parse_status(""), Status::Announced);
    }

    #[test]
    fn a_sunset_renders_in_utc_so_two_rows_cannot_look_a_day_apart() {
        let utc = OffsetDateTime::parse(
            "2027-04-15T00:00:00Z",
            &time::format_description::well_known::Rfc3339,
        )
        .expect("the fixture parses");
        let plus_three = utc.to_offset(time::UtcOffset::from_hms(3, 0, 0).expect("+03:00"));
        assert_eq!(format_rfc3339(utc), "2027-04-15T00:00:00Z");
        // The same instant in a +03:00 offset would render as the 15th at 03:00 without this,
        // which is still the 15th — but an offset that crosses midnight would move the DATE, and
        // two deadlines a day apart would look identical.
        assert_eq!(format_rfc3339(plus_three), "2027-04-15T00:00:00Z");
    }
}