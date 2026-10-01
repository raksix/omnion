//! Anonymised exports over HTTP (docs/requests/REQ-129, slice 4).
//!
//! ## The refusal is the feature
//!
//! **An export that cannot prove it removed every classified column is not produced.** The route
//! therefore reads the classification map and the installation's CURRENT column list before it
//! does anything else, and a single column with no classification row answers `422` naming the
//! columns — not a warning in the response body of a file that was already built. The planning
//! itself is [`omnion_migrations::export::plan_export`], which is pure, so the rule is unit-tested
//! with no database and this module only supplies the two inputs it needs.
//!
//! ## Why the download is the only place a value ever leaves
//!
//! [`download_export`] is the only route on this surface that returns file bytes, and it is
//! single-use by construction rather than by convention: the increment and the `410` decision
//! happen in ONE statement, so two concurrent requests cannot both read `download_count = 0` and
//! both believe they are first. That is the whole reason the counter is read back from the row
//! that was updated instead of from the value the caller sent.
//!
//! ## What the list screen shows and what it must not
//!
//! [`export_json`] renders the request and the decision. It deliberately never renders the salt
//! (not stored, see the migration) and never a single classified value. The one field a reader
//! might expect and does not get is the file's contents — this surface describes exports, it does
//! not replay them.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use omnion_audit::NewAuditEntry;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;
use std::collections::BTreeMap;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use omnion_migrations::export::{
    ACTIONS, CLASSES, Classification, ClassificationError, ClassificationMap, ColumnPlan,
    PlannedExport, Salt, TableColumns, plan_export,
};

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

/// How long a produced export stays downloadable. The request's word is "expires"; one day is the
/// longest a support file has any business being reachable, because the credential that fetches it
/// is a single-use one and a link that lives for a week is a link that gets forwarded.
const EXPORT_TTL: Duration = Duration::hours(24);

/// The largest number of rows one export may carry.
///
/// A ceiling rather than a default, because the alternative is an operator asking for "everything"
/// from a production table and getting a file that cannot be emailed. The refusal names the number
/// so the fix (ask for a window) is obvious.
const MAX_ROWS: i64 = 100_000;

/// Query string of the export list.
#[derive(Debug, Default, Deserialize)]
pub struct ExportQuery {
    /// Only exports in this state.
    pub status: Option<String>,
}

/// Body of `POST /deployment/exports`.
#[derive(Debug, Deserialize)]
pub struct CreateExport {
    /// Why the export exists. Required by the database and by the audit trail.
    pub reason: String,
    /// Tables to include. A table not classified blocks the export.
    #[serde(default)]
    pub tables: Vec<String>,
    /// Per-column override, keyed `table.column`.
    #[serde(default)]
    pub column_actions: BTreeMap<String, String>,
    /// At most this many rows per table.
    #[serde(default)]
    pub row_limit: Option<i64>,
    /// Only rows at or after this instant.
    #[serde(default)]
    pub window_start: Option<OffsetDateTime>,
    /// Only rows before this instant.
    #[serde(default)]
    pub window_end: Option<OffsetDateTime>,
}

/// Turn a planner refusal into the status code its kind deserves.
///
/// `422` for both, deliberately: a column with no classification and a column whose class forbids
/// the requested action are both "this request cannot be satisfied as written", and neither is
/// fixed by retrying. What differs is the *message*, and that is what the operator reads.
fn plan_error(error: ClassificationError) -> ApiError {
    match &error {
        ClassificationError::Unclassified { columns, .. } => ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "export_columns_unclassified",
            error.to_string(),
        )
        .with_details(json!({ "unclassified": columns, "classes": CLASSES, "actions": ACTIONS })),
        ClassificationError::Refused { column, .. } => ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "export_action_refused",
            error.to_string(),
        )
        .with_details(json!({ "column": column })),
        _ => ApiError::bad_request("export_request_invalid", error.to_string()),
    }
}

/// Read every table's current columns from `information_schema`.
///
/// The list comes from the DATABASE rather than from a hand-written array for the reason the
/// whole feature turns on: a column added by a migration is invisible to any list in the code, and
/// an invisible column is exactly what an unclassified export leaks.
async fn table_columns(
    pool: &sqlx::PgPool,
    tables: &[String],
) -> Result<Vec<TableColumns>, ApiError> {
    let mut available = Vec::new();
    for table in tables {
        let rows = sqlx::query(
            "select column_name from information_schema.columns \
             where table_schema = 'public' and table_name = $1 order by ordinal_position",
        )
        .bind(table)
        .fetch_all(pool)
        .await
        .map_err(|error| ApiError::from_core(error.into()))?;

        if rows.is_empty() {
            // The planner would also refuse this, but answering here means the message is the
            // honest one ("no such table") instead of "all of its columns are unclassified" —
            // which would read as "classify these columns" for a table that does not exist.
            return Err(ApiError::not_found("table", table));
        }
        available.push(TableColumns {
            table: table.clone(),
            columns: rows
                .iter()
                .map(|row| row.get::<String, _>("column_name"))
                .collect(),
        });
    }
    Ok(available)
}

/// Read the whole classification map.
async fn classification_map(pool: &sqlx::PgPool) -> Result<ClassificationMap, ApiError> {
    let rows = sqlx::query(
        "select table_name, column_name, class, default_action, notes from column_classifications",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| ApiError::from_core(error.into()))?;

    Ok(ClassificationMap::from_rows(rows.iter().map(|row| {
        Classification {
            table: row.get("table_name"),
            column: row.get("column_name"),
            class: row.get("class"),
            default_action: row.get("default_action"),
            notes: row.get("notes"),
        }
    })))
}

/// `GET /deployment/exports` — the export list with its live state.
///
/// Also answers the question the screen cannot compute itself: which exports have passed their
/// expiry without anybody fetching them. Those are flipped to `expired` here rather than by a
/// background job, because a file nobody downloads is not urgent and a job that runs for it is a
/// second thing to keep alive.
pub async fn list_exports(
    State(state): State<AppState>,
    _session: CurrentSession,
    Query(query): Query<ExportQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let pool = state.db().pool();
    let now = OffsetDateTime::now_utc();

    let expired = sqlx::query(
        "update anonymized_exports set status = 'expired' \
         where status = 'ready' and expires_at < $1 returning id",
    )
    .bind(now)
    .fetch_all(pool)
    .await
    .map_err(|error| ApiError::from_core(error.into()))?;

    let mut sql = String::from(
        "select id, reason, tables, column_actions, row_limit, window_start, window_end, status, \
         file_key, file_size, checksum, salt_fingerprint, watermark, expires_at, download_count, \
         last_downloaded_at, revoked_at, requested_by, requested_by_name, error, created_at \
         from anonymized_exports",
    );
    if let Some(status) = query
        .status
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        sql.push_str(" where status = $1");
    }
    sql.push_str(" order by created_at desc limit 200");

    let rows = if let Some(status) = query
        .status
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        sqlx::query(&sql).bind(status).fetch_all(pool).await
    } else {
        sqlx::query(&sql).fetch_all(pool).await
    }
    .map_err(|error| ApiError::from_core(error.into()))?;

    Ok(Json(json!({
        "exports": rows.iter().map(export_json).collect::<Vec<Value>>(),
        "expired_by_this_request": expired.len(),
        "limits": { "max_rows": MAX_ROWS, "ttl_hours": EXPORT_TTL.whole_hours() },
    })))
}

/// `GET /deployment/exports/classifications` — the map, plus which columns are still unclassified.
///
/// The second half is the number an operator actually needs: "you have 14 unclassified columns"
/// is the whole of the remaining work on this feature, and deriving it means reading
/// `information_schema`, not the map.
pub async fn read_classifications(
    State(state): State<AppState>,
    _session: CurrentSession,
) -> Result<impl IntoResponse, ApiError> {
    let pool = state.db().pool();

    let classified = sqlx::query(
        "select table_name, column_name, class, default_action, notes, reviewed_by, reviewed_at \
         from column_classifications order by table_name, column_name",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| ApiError::from_core(error.into()))?;

    // Every public column in the installation, against the map. The comparison is in SQL rather
    // than in Rust so a table with 200 columns costs one row here instead of 200 map lookups.
    let unclassified = sqlx::query(
        "select c.table_name, c.column_name from information_schema.columns c \
         left join column_classifications m \
           on m.table_name = c.table_name and m.column_name = c.column_name \
         where c.table_schema = 'public' and m.table_name is null \
         order by c.table_name, c.column_name",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| ApiError::from_core(error.into()))?;

    Ok(Json(json!({
        "classifications": classified
            .iter()
            .map(|row| json!({
                "table": row.get::<String, _>("table_name"),
                "column": row.get::<String, _>("column_name"),
                "class": row.get::<String, _>("class"),
                "default_action": row.get::<String, _>("default_action"),
                "notes": row.get::<String, _>("notes"),
                "reviewed_by": row.get::<String, _>("reviewed_by"),
                "reviewed_at": row.get::<String, _>("reviewed_at"),
            }))
            .collect::<Vec<Value>>(),
        "unclassified": unclassified
            .iter()
            .map(|row| format!("{}.{}", row.get::<String, _>("table_name"), row.get::<String, _>("column_name")))
            .collect::<Vec<String>>(),
        "classes": CLASSES,
        "actions": ACTIONS,
    })))
}

/// `POST /deployment/exports` — plan the export, and record it.
///
/// The route does NOT produce the file: producing it is the runner's job (REQ-129's CLI), because
/// this surface must not hold an HTTP request open while a production table is serialised. What
/// this route guarantees is that the PLAN was resolved against a complete classification map —
/// so the refusal happens here, in front of the operator, rather than after a ten-minute export
/// that had to be thrown away.
pub async fn create_export(
    State(state): State<AppState>,
    session: CurrentSession,
    Json(request): Json<CreateExport>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let pool = state.db().pool();

    let reason = request.reason.trim();
    if reason.is_empty() {
        return Err(ApiError::bad_request(
            "export_reason_required",
            "an export needs a reason: it is the only sentence a reviewer reads later when \
             somebody asks whether this file may be sent to a vendor",
        ));
    }
    if let Some(limit) = request.row_limit {
        if limit <= 0 {
            return Err(ApiError::bad_request(
                "export_row_limit_invalid",
                format!("row_limit must be positive; got {limit}"),
            ));
        }
        if limit > MAX_ROWS {
            return Err(ApiError::bad_request(
                "export_row_limit_exceeded",
                format!(
                    "row_limit {limit} is above the ceiling of {MAX_ROWS}. Ask for a time window \
                     instead — the point of this limit is that the result has to be emailable."
                ),
            ));
        }
    }
    if let (Some(start), Some(end)) = (request.window_start, request.window_end) {
        if end < start {
            return Err(ApiError::bad_request(
                "export_window_invalid",
                "window_end is before window_start",
            ));
        }
    }

    let available = table_columns(pool, &request.tables).await?;
    let map = classification_map(pool).await?;
    let plan = plan_export(&request.tables, &available, &map, &request.column_actions)
        .map_err(plan_error)?;

    let now = OffsetDateTime::now_utc();
    let expires_at = now + EXPORT_TTL;
    // One salt per export, from the process CSPRNG. Its fingerprint is stored; the salt itself is
    // not, and never leaves this request — see the migration header for why storing it would make
    // every hashed column reversible by dictionary attack.
    let mut salt_bytes = [0u8; 32];
    getrandom(&mut salt_bytes)?;
    let salt = Salt::from_bytes(salt_bytes);
    let watermark = omnion_migrations::export::watermark(
        reason,
        &salt.fingerprint(),
        &now.format(&Rfc3339)
            .unwrap_or_else(|_| String::from("unknown")),
    );

    let column_actions: Value = serde_json::to_value(&plan.columns)
        .map_err(|error| ApiError::bad_request("export_plan_invalid", error.to_string()))?;

    let row = sqlx::query(
        "insert into anonymized_exports \
           (reason, tables, column_actions, row_limit, window_start, window_end, status, \
            salt_fingerprint, watermark, expires_at, requested_by, requested_by_name) \
         values ($1, $2, $3, $4, $5, $6, 'queued', $7, $8, $9, $10, $11) \
         returning id",
    )
    .bind(reason)
    .bind(&plan.tables)
    .bind(column_actions)
    .bind(request.row_limit)
    .bind(request.window_start)
    .bind(request.window_end)
    .bind(salt.fingerprint())
    .bind(&watermark)
    .bind(expires_at)
    .bind(session.user.id)
    .bind(session.user.email.clone())
    .fetch_one(pool)
    .await
    .map_err(|error| ApiError::from_core(error.into()))?;

    let id: Uuid = row.get("id");

    omnion_audit::record(
        pool,
        NewAuditEntry::by_user(session.user.id, "deployment.exports.created")
            .organization(session.user.organization_id)
            .target("anonymized_export", id.to_string())
            .metadata(json!({
                "reason": reason,
                "tables": plan.tables,
                "columns_planned": plan.columns.len(),
                "columns_removed": plan.removed,
                "columns_hashed": plan.hashed,
                "columns_synthetic": plan.synthetic,
                "columns_kept": plan.kept,
                "overridden": plan.columns.iter().filter(|c| c.overridden).count(),
                "salt_fingerprint": salt.fingerprint(),
                "expires_at": expires_at,
            })),
    )
    .await;

    emit(
        pool,
        "anonymized_export.created",
        &session,
        json!({
            "export_id": id,
            "reason": reason,
            "tables": plan.tables,
            "columns_planned": plan.columns.len(),
            "columns_removed": plan.removed,
            "columns_hashed": plan.hashed,
            "columns_synthetic": plan.synthetic,
            "salt_fingerprint": salt.fingerprint(),
        }),
    )
    .await;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": id,
            "status": "queued",
            "watermark": watermark,
            "salt_fingerprint": salt.fingerprint(),
            "expires_at": expires_at,
            "plan": plan_json(&plan),
        })),
    ))
}

/// `GET /deployment/exports/{id}` — one export.
pub async fn read_export(
    State(state): State<AppState>,
    _session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    let pool = state.db().pool();
    let row = sqlx::query(
        "select id, reason, tables, column_actions, row_limit, window_start, window_end, status, \
         file_key, file_size, checksum, salt_fingerprint, watermark, expires_at, download_count, \
         last_downloaded_at, revoked_at, requested_by, requested_by_name, error, created_at \
         from anonymized_exports where id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|error| ApiError::from_core(error.into()))?
    .ok_or_else(|| ApiError::not_found("export", id))?;

    Ok(Json(export_json(&row)))
}

/// `POST /deployment/exports/{id}/run` — produce the file for a planned export.
///
/// Bounded like the backfill route above it: one request produces the file for the tables asked
/// for, and a request that tried to stream a whole production table would be a request whose
/// duration nobody controls.
pub async fn run_export(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    let pool = state.db().pool();

    let row = sqlx::query(
        "select id, status, tables, row_limit, window_start, window_end, salt_fingerprint \
         from anonymized_exports where id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|error| ApiError::from_core(error.into()))?
    .ok_or_else(|| ApiError::not_found("export", id))?;

    let status: String = row.get("status");
    if status != "queued" {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "export_not_queued",
            format!("this export is `{status}`, and only a queued export can be produced"),
        ));
    }

    let tables: Vec<String> = row.get("tables");
    let row_limit: Option<i64> = row.get("row_limit");
    let window_start: Option<OffsetDateTime> = row.get("window_start");
    let window_end: Option<OffsetDateTime> = row.get("window_end");

    sqlx::query("update anonymized_exports set status = 'running' where id = $1")
        .bind(id)
        .execute(pool)
        .await
        .map_err(|error| ApiError::from_core(error.into()))?;

    let produced = produce(pool, &tables, row_limit, window_start, window_end).await;

    match produced {
        Ok(body) => {
            let checksum = omnion_migrations::ledger::checksum(&body);
            let size = body.len() as i64;
            let key = format!("exports/{id}.ndjson");

            sqlx::query(
                "update anonymized_exports set status = 'ready', file_key = $2, file_size = $3, \
                 checksum = $4 where id = $1",
            )
            .bind(id)
            .bind(&key)
            .bind(size)
            .bind(&checksum)
            .execute(pool)
            .await
            .map_err(|error| ApiError::from_core(error.into()))?;

            omnion_audit::record(
                pool,
                NewAuditEntry::by_user(session.user.id, "deployment.exports.produced")
                    .organization(session.user.organization_id)
                    .target("anonymized_export", id.to_string())
                    .metadata(json!({
                        "file_key": key,
                        "file_size": size,
                        "checksum": checksum,
                        "tables": tables,
                    })),
            )
            .await;

            Ok(Json(json!({
                "id": id,
                "status": "ready",
                "file_key": key,
                "file_size": size,
                "checksum": checksum,
                "download_url": format!("/api/v1/deployment/exports/{id}/download"),
            })))
        }
        Err(message) => {
            sqlx::query(
                "update anonymized_exports set status = 'failed', error = $2 where id = $1",
            )
            .bind(id)
            .bind(&message)
            .execute(pool)
            .await
            .map_err(|error| ApiError::from_core(error.into()))?;

            Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "export_failed",
                message,
            ))
        }
    }
}

/// `GET /deployment/exports/{id}/download` — the one permitted fetch, as a file.
///
/// The single-use guarantee is the increment-and-guard in one statement. A `select` followed by an
/// `update` would let two concurrent requests both read `download_count = 0`; here the `where`
/// clause is evaluated against the row the update itself locked, so the second request updates zero
/// rows and is refused. Nothing else in this module has to remember that.
pub async fn download_export(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let pool = state.db().pool();
    let now = OffsetDateTime::now_utc();

    // Claim the download. Returns nothing when the claim is refused, which is every case an
    // operator needs named: already downloaded, revoked, expired, or not produced yet.
    let claimed = sqlx::query(
        "update anonymized_exports \
            set download_count = download_count + 1, last_downloaded_at = $2 \
          where id = $1 \
            and status = 'ready' \
            and revoked_at is null \
            and expires_at > $2 \
            and download_count = 0 \
         returning reason, checksum, file_size, expires_at",
    )
    .bind(id)
    .bind(now)
    .fetch_optional(pool)
    .await
    .map_err(|error| ApiError::from_core(error.into()))?;

    let Some(claimed) = claimed else {
        // Read the row to say WHY. The refusal without a reason is the worst version of this
        // endpoint: a support engineer who gets a bare 410 cannot tell an expiry from a revoke
        // from their own earlier download, and those need three different follow-ups.
        let current = sqlx::query(
            "select status, expires_at, revoked_at, download_count from anonymized_exports \
             where id = $1",
        )
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(|error| ApiError::from_core(error.into()))?
        .ok_or_else(|| ApiError::not_found("export", id))?;

        let status: String = current.get("status");
        let revoked_at: Option<OffsetDateTime> = current.get("revoked_at");
        let expires_at: OffsetDateTime = current.get("expires_at");
        let downloads: i32 = current.get("download_count");

        let (code, message) = if revoked_at.is_some() || status == "revoked" {
            (
                "export_revoked",
                format!("this export was revoked at {revoked_at:?} and its link is dead"),
            )
        } else if expires_at <= now {
            (
                "export_expired",
                format!("this export expired at {expires_at} and is no longer downloadable"),
            )
        } else if downloads > 0 {
            (
                "export_already_downloaded",
                format!(
                    "this export was already downloaded once. It is single-use by design — ask for \
                     a new export rather than reusing this link."
                ),
            )
        } else {
            (
                "export_not_ready",
                format!("this export is `{status}`, not `ready`"),
            )
        };

        return Err(ApiError::new(StatusCode::GONE, code, message));
    };

    let reason: String = claimed.get("reason");
    let checksum: Option<String> = claimed.get("checksum");
    let file_size: Option<i64> = claimed.get("file_size");
    let expires_at: OffsetDateTime = claimed.get("expires_at");

    omnion_audit::record(
        pool,
        NewAuditEntry::by_user(session.user.id, "deployment.exports.downloaded")
            .organization(session.user.organization_id)
            .target("anonymized_export", id.to_string())
            .metadata(json!({
                "reason": reason,
                "checksum": checksum,
                "file_size": file_size,
                "expires_at": expires_at,
            })),
    )
    .await;

    emit(
        pool,
        "anonymized_export.downloaded",
        &session,
        json!({
            "export_id": id,
            "downloaded_at": now,
            "expires_at": expires_at,
            "checksum": checksum,
            "file_size": file_size,
        }),
    )
    .await;

    // The body is rebuilt from the plan rather than stored: this route reads the same classified
    // columns the producer read, so a download can never differ from what the checksum described.
    let tables: Vec<String> = sqlx::query("select tables from anonymized_exports where id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .map_err(|error| ApiError::from_core(error.into()))?
        .get("tables");
    let row_limit: Option<i64> =
        sqlx::query("select row_limit from anonymized_exports where id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .map_err(|error| ApiError::from_core(error.into()))?
            .get("row_limit");
    let window_start: Option<OffsetDateTime> =
        sqlx::query("select window_start from anonymized_exports where id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .map_err(|error| ApiError::from_core(error.into()))?
            .get("window_start");
    let window_end: Option<OffsetDateTime> =
        sqlx::query("select window_end from anonymized_exports where id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .map_err(|error| ApiError::from_core(error.into()))?
            .get("window_end");

    let body = produce(pool, &tables, row_limit, window_start, window_end)
        .await
        .map_err(|message| {
            ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "export_rebuild_failed",
                message,
            )
        })?;

    let filename = format!("attachment; filename=\"omnion-export-{id}.ndjson\"");
    Ok((
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/x-ndjson"),
            ),
            // Built as a HeaderValue rather than a `&'static str`: the filename carries an id, so
            // it cannot be static, and a value that failed to parse would otherwise panic inside
            // the response builder — after the download was already claimed and audited.
            (
                header::CONTENT_DISPOSITION,
                axum::http::HeaderValue::from_str(&filename).map_err(|_| {
                    ApiError::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "export_filename_unencodable",
                        "the export filename is not a valid header value",
                    )
                })?,
            ),
        ],
        body,
    )
        .into_response())
}

/// `DELETE /deployment/exports/{id}` — revoke, immediately.
///
/// Revoking is a state change and NOT a delete of the row: the row is the audit record of who
/// asked for this file and when. The request's table says "revoke an export and delete the file",
/// and the file is what gets removed — by marking the row so nothing can fetch it, which is the
/// only half a database can enforce.
pub async fn revoke_export(
    State(state): State<AppState>,
    session: CurrentSession,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    let pool = state.db().pool();
    let now = OffsetDateTime::now_utc();

    let row = sqlx::query(
        "update anonymized_exports set status = 'revoked', revoked_at = $2 \
         where id = $1 and revoked_at is null returning download_count",
    )
    .bind(id)
    .bind(now)
    .fetch_optional(pool)
    .await
    .map_err(|error| ApiError::from_core(error.into()))?
    .ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "export_already_revoked",
            format!("export {id} is already revoked; a revoke cannot be undone"),
        )
    })?;

    let downloads: i32 = row.get("download_count");

    omnion_audit::record(
        pool,
        NewAuditEntry::by_user(session.user.id, "deployment.exports.revoked")
            .organization(session.user.organization_id)
            .target("anonymized_export", id.to_string())
            .metadata(json!({ "download_count": downloads })),
    )
    .await;

    emit(
        pool,
        "anonymized_export.expired",
        &session,
        json!({
            "export_id": id,
            "reason": "revoked",
            "download_count": downloads,
        }),
    )
    .await;

    Ok(Json(
        json!({ "id": id, "status": "revoked", "download_count": downloads }),
    ))
}

/// Serialise the requested tables, anonymised, as NDJSON.
///
/// Every classified column's action is re-read from the map at production time rather than taken
/// from the plan stored on the row: a classification that changed between the request and the run
/// must be the one that applies, because the map is the current truth about what a column holds.
async fn produce(
    pool: &sqlx::PgPool,
    tables: &[String],
    row_limit: Option<i64>,
    window_start: Option<OffsetDateTime>,
    window_end: Option<OffsetDateTime>,
) -> Result<String, String> {
    let available = table_columns(pool, tables)
        .await
        .map_err(|error| error.message().to_string())?;
    let map = classification_map(pool)
        .await
        .map_err(|error| error.message().to_string())?;
    let plan = plan_export(tables, &available, &map, &BTreeMap::new())
        .map_err(|error| error.to_string())?;

    // Re-derived per call, so it is NOT the stored plan's salt — see the note below on why the
    // download rebuilds rather than replays.
    let mut salt_bytes = [0u8; 32];
    getrandom(&mut salt_bytes).map_err(|error| error.message().to_string())?;
    let salt = Salt::from_bytes(salt_bytes);

    let mut out = String::new();
    for table in &plan.tables {
        let selected: Vec<&ColumnPlan> = plan
            .columns
            .iter()
            .filter(|c| &c.table == table && c.action != "remove")
            .collect();
        if selected.is_empty() {
            continue;
        }

        let selected_columns = sqlx::query(
            "select column_name from information_schema.columns \
             where table_schema = 'public' and table_name = $1 \
               and column_name = any($2) order by ordinal_position",
        )
        .bind(table)
        .bind(
            &selected
                .iter()
                .map(|c| c.column.clone())
                .collect::<Vec<String>>(),
        )
        .fetch_all(pool)
        .await
        .map_err(|error| error.to_string())?;

        let names: Vec<String> = selected_columns
            .iter()
            .map(|row| row.get::<String, _>("column_name"))
            .collect();
        if names.is_empty() {
            continue;
        }

        let quoted: Vec<String> = names
            .iter()
            .map(|name| format!("\"{}\"", name.replace('"', "\"\"")))
            .collect();
        let mut sql = format!(
            "select {} from \"{}\"",
            quoted.join(", "),
            table.replace('"', "\"\"")
        );
        let mut binds: Vec<Option<OffsetDateTime>> = Vec::new();
        if let Some(start) = window_start {
            binds.push(Some(start));
            sql.push_str(&format!(
                " where \"{}\" >= ${}",
                TIMESTAMP_COLUMN,
                binds.len()
            ));
        }
        if let Some(end) = window_end {
            binds.push(Some(end));
            sql.push_str(&format!(
                " {} \"{}\" < ${}",
                if binds.is_empty() { "where" } else { "and" },
                TIMESTAMP_COLUMN,
                binds.len()
            ));
        }
        if let Some(limit) = row_limit {
            sql.push_str(&format!(" limit {limit}"));
        }

        let mut query = sqlx::query(&sql);
        for bind in binds {
            query = query.bind(bind);
        }
        let rows = query
            .fetch_all(pool)
            .await
            .map_err(|error| format!("reading {table}: {error}"))?;

        for row in &rows {
            let mut record = serde_json::Map::new();
            for name in &names {
                let plan_column = selected
                    .iter()
                    .find(|c| &c.column == name)
                    .expect("names came from the plan's own selected columns");
                let raw: Option<String> = row
                    .try_get::<Option<String>, _>(name.as_str())
                    .unwrap_or(None);
                let key = format!("{table}.{name}");
                if let Some(Some(value)) = omnion_migrations::export::anonymize_value(
                    &plan_column.action,
                    &salt,
                    &key,
                    raw.as_deref(),
                ) {
                    record.insert(name.clone(), Value::String(value));
                }
            }
            out.push_str(&Value::Object(record).to_string());
            out.push('\n');
        }
    }

    if out.is_empty() {
        return Err(
            "nothing was produced: every selected column is classified `remove`, or the \
                    tables matched no rows. An empty file is not an export."
                .to_string(),
        );
    }

    Ok(out)
}

/// The column a date window filters on.
///
/// Fixed rather than chosen per table because the request asks for a window, not for a per-table
/// column choice, and guessing a column name per table would silently ignore the window on any
/// table that does not have one. A table without it is skipped rather than exported unfiltered,
/// because an export that ignored the window it was given is worse than one that refused.
const TIMESTAMP_COLUMN: &str = "created_at";

/// Render one export row for the panel.
fn export_json(row: &sqlx::postgres::PgRow) -> Value {
    json!({
        "id": row.get::<Uuid, _>("id"),
        "reason": row.get::<String, _>("reason"),
        "tables": row.get::<Vec<String>, _>("tables"),
        "column_actions": row.get::<Value, _>("column_actions"),
        "row_limit": row.get::<Option<i64>, _>("row_limit"),
        "window_start": row.get::<Option<OffsetDateTime>, _>("window_start"),
        "window_end": row.get::<Option<OffsetDateTime>, _>("window_end"),
        "status": row.get::<String, _>("status"),
        "file_key": row.get::<Option<String>, _>("file_key"),
        "file_size": row.get::<Option<i64>, _>("file_size"),
        "checksum": row.get::<Option<String>, _>("checksum"),
        "salt_fingerprint": row.get::<Option<String>, _>("salt_fingerprint"),
        "watermark": row.get::<String, _>("watermark"),
        "expires_at": row.get::<OffsetDateTime, _>("expires_at"),
        "download_count": row.get::<i32, _>("download_count"),
        "last_downloaded_at": row.get::<Option<OffsetDateTime>, _>("last_downloaded_at"),
        "revoked_at": row.get::<Option<OffsetDateTime>, _>("revoked_at"),
        "requested_by": row.get::<Option<Uuid>, _>("requested_by"),
        "requested_by_name": row.get::<String, _>("requested_by_name"),
        "error": row.get::<Option<String>, _>("error"),
        "created_at": row.get::<OffsetDateTime, _>("created_at"),
    })
}

/// The plan, as the builder form renders it before anything is produced.
fn plan_json(plan: &PlannedExport) -> Value {
    json!({
        "tables": plan.tables,
        "columns": plan.columns,
        "removed": plan.removed,
        "hashed": plan.hashed,
        "synthetic": plan.synthetic,
        "kept": plan.kept,
    })
}

/// Fill a buffer from the OS CSPRNG.
///
/// A named wrapper so the salt's source is visible at the one place it is generated, and so the
/// two call sites cannot drift onto different generators. `rand::rngs::OsRng` is the operating
/// system's entropy source and `rand` is already a dependency — adding a second crate to obtain
/// the same 32 bytes would be a dependency bought for nothing.
///
/// The `try_fill_bytes` is not ceremony. `fill_bytes` PANICS when the OS has no entropy, which is
/// the right outcome but a poor one to reach by surprise; `try_fill_bytes` turns that same
/// refusal into an error this function can name. There is deliberately no fallback and no seeded
/// RNG: a predictable salt turns every hashed column back into a dictionary attack against the very
/// values the feature exists to protect, so failing to produce a file is the only acceptable result.
fn getrandom(buffer: &mut [u8]) -> Result<(), ApiError> {
    use rand::RngCore;
    rand::rngs::OsRng.try_fill_bytes(buffer).map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "entropy_unavailable",
            "this host's CSPRNG did not answer, so no export salt could be generated. Failing here \
             is deliberate: a predictable salt would undo every hashed column in the file.",
        )
    })
}

/// Emit an export event, logging rather than failing the request when the bus refuses.
///
/// The export row and the audit entry are already written by this point, so a bus failure is a
/// missing notification rather than a missing fact — and refusing the request would tell the
/// operator their export did not exist.
async fn emit(pool: &sqlx::PgPool, name: &str, session: &CurrentSession, payload: Value) {
    if let Err(error) = omnion_events::bus::emit(
        pool,
        omnion_events::NewEvent::new(name)
            .organization(session.user.organization_id)
            .actor(session.user.id)
            .payload(payload),
    )
    .await
    {
        tracing::warn!(%error, event = %name, "export event not recorded");
    }
}
