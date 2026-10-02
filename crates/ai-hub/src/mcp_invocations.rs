//! The MCP invocation log — every call, granted or refused (REQ-108, slice 2).
//!
//! # What this table is for
//!
//! An MCP client is a machine user holding a bearer token, and the request calls this area "the
//! highest-value target in the platform for lateral movement". That makes this table the one
//! thing that must be complete: if a call happened and produced no row, the platform cannot say
//! what an agent did. So the row is written on **every** path — allowed, denied, parked and
//! sandboxed — and never skipped for being inconvenient.
//!
//! # The arguments are masked before they are stored, and the preview is small on purpose
//!
//! `arguments_preview` holds REQ-105's masked text and the argument **keys**, never the values. A
//! client that reads a customer's address into a page title has just moved that address into a
//! table with a different retention path; the guard decides what may be written, and the keys are
//! enough for an operator to see which call it was. The unmasked digest is derived from the
//! arguments before masking, so two identical calls still group together without the table
//! holding either one's values.
//!
//! # Rows outlive their client, unless it is deleted on purpose
//!
//! `client_id` cascades. That is right for a client created by mistake and wrong for a deleted
//! production client — so the delete route is the only way to lose history, and it is a typed
//! confirmation, not a stray `DELETE`. Revocation (slice 1) keeps the row and the whole history,
//! which is the case the acceptance criteria actually name.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::Result;

/// The statuses a row may carry.
///
/// The first four are the migration's constraint; `pending_approval` is the fifth this slice adds
/// for a gated tool, and it is a **status rather than a separate table** on purpose: the
/// acceptance criterion says "the invocation row moves to `ok`" after the approval, which is
/// only expressible if the row that parked is the row that succeeds.
pub const STATUSES: &[&str] = &["ok", "error", "denied", "sandbox", "blocked_airgap", "pending_approval"];

/// One invocation, as the panel's list and detail render it.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct InvocationRow {
    pub id: i64,
    pub organization_id: Uuid,
    /// `None` after the client row was deleted; the table still names the tool and the outcome.
    pub client_id: Option<Uuid>,
    pub jsonrpc_id: Option<String>,
    pub tool: String,
    pub permission: Option<String>,
    pub arguments_sha256: String,
    pub arguments_preview: Value,
    pub status: String,
    pub error_code: Option<String>,
    pub duration_ms: i32,
    pub approval_id: Option<Uuid>,
    pub run_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl InvocationRow {
    /// Whether this row is a success, for the status badge and the failure rate.
    ///
    /// `sandbox` counts as **not** a success and **not** a failure: a sandboxed call proves the
    /// plan and writes nothing, so including it in either column would make a client that only
    /// rehearses look either broken or alarmingly productive. The screen shows the breakdown;
    /// this predicate answers "did a write happen".
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.status == "ok"
    }

    /// Whether a human has to act before this call can finish.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        self.status == "pending_approval"
    }
}

/// A row to append.
#[derive(Debug, Clone)]
pub struct NewInvocation {
    pub organization_id: Uuid,
    pub client_id: Option<Uuid>,
    pub jsonrpc_id: Option<String>,
    pub tool: String,
    pub permission: Option<String>,
    pub arguments_sha256: String,
    pub arguments_preview: Value,
    pub status: String,
    pub error_code: Option<String>,
    pub duration_ms: i32,
    pub approval_id: Option<Uuid>,
    pub run_id: Option<Uuid>,
}

/// The list filter.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct InvocationFilter {
    /// Only this client's calls.
    pub client_id: Option<Uuid>,
    /// Only this tool's calls.
    pub tool: Option<String>,
    /// Only this status.
    pub status: Option<String>,
    /// Only calls at or after `now - window_seconds`.
    pub window_seconds: Option<i64>,
    /// Keyset pagination: rows older than this id.
    pub before: Option<i64>,
    pub limit: Option<i64>,
}

/// One page plus the cursor.
#[derive(Debug, Clone, Serialize)]
pub struct InvocationPage {
    pub invocations: Vec<InvocationRow>,
    /// The `before` value for the next page, `None` at the end.
    pub next_before: Option<i64>,
}

/// The window a call count covers when the screen asks for no particular one.
pub const DEFAULT_WINDOW_SECONDS: i64 = 24 * 60 * 60;

/// The largest page the store will serve.
///
/// A log screen with no ceiling is an export endpoint wearing a table's clothes: 200 is more than
/// a person reads, and the walkthrough clicks "load more" rather than scrolling 40 000 rows.
pub const MAX_PAGE: i64 = 200;

const COLUMNS: &str = "id, organization_id, client_id, jsonrpc_id, tool, permission, \
     arguments_sha256, arguments_preview, status, error_code, duration_ms, approval_id, run_id, \
     created_at";

/// Append one invocation.
///
/// A status outside [`STATUSES`] is refused **by the store** rather than by the migration's check
/// constraint alone: the constraint reports `check_violation` with no hint that six names exist,
/// which is the same failure `guard_store::record_event` had with its action vocabulary — the
/// message pointed at the database and not at the caller that filed the wrong word.
pub async fn record(pool: &PgPool, new: NewInvocation) -> Result<i64> {
    if !STATUSES.contains(&new.status.as_str()) {
        return Err(crate::error::AiHubError::InvalidMcpClient(format!(
            "`{}` is not an invocation status; the vocabulary is {}",
            new.status,
            STATUSES.join(", ")
        )));
    }
    let row: (i64,) = sqlx::query_as(
        "insert into mcp_invocations (organization_id, client_id, jsonrpc_id, tool, permission, \
           arguments_sha256, arguments_preview, status, error_code, duration_ms, approval_id, \
           run_id) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
         returning id",
    )
    .bind(new.organization_id)
    .bind(new.client_id)
    .bind(new.jsonrpc_id.as_deref())
    .bind(&new.tool)
    .bind(new.permission.as_deref())
    .bind(&new.arguments_sha256)
    .bind(&new.arguments_preview)
    .bind(&new.status)
    .bind(new.error_code.as_deref())
    .bind(new.duration_ms.max(0))
    .bind(new.approval_id)
    .bind(new.run_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// Move a parked invocation to its final status.
///
/// Used by the approval path: the criterion is that the row "moves to `ok`", and a second row
/// would make the history read as two calls. `where status = 'pending_approval'` is the guard,
/// so a re-approval cannot rewrite an invocation that has already finished — the walk asserts
/// that the second attempt changes nothing.
pub async fn settle(
    pool: &PgPool,
    id: i64,
    status: &str,
    error_code: Option<&str>,
) -> Result<u64> {
    if !STATUSES.contains(&status) {
        return Err(crate::error::AiHubError::InvalidMcpClient(format!(
            "`{status}` is not an invocation status"
        )));
    }
    let done = sqlx::query(
        "update mcp_invocations set status = $2, error_code = $3 \
          where id = $1 and status = 'pending_approval'",
    )
    .bind(id)
    .bind(status)
    .bind(error_code)
    .execute(pool)
    .await?;
    Ok(done.rows_affected())
}

/// A page of the log, newest first.
pub async fn search(
    pool: &PgPool,
    organization_id: Uuid,
    filter: &InvocationFilter,
) -> Result<InvocationPage> {
    let limit = filter.limit.unwrap_or(50).clamp(1, MAX_PAGE);
    let window = filter.window_seconds.unwrap_or(DEFAULT_WINDOW_SECONDS).max(1);
    let rows: Vec<InvocationRow> = sqlx::query_as(&format!(
        "select {COLUMNS} from mcp_invocations \
          where organization_id = $1 \
            and ($2::uuid is null or client_id = $2) \
            and ($3::text is null or tool = $3) \
            and ($4::text is null or status = $4) \
            and ($5::bigint is null or created_at > now() - make_interval(secs => $5::bigint)) \
            and ($6::bigint is null or id < $6) \
          order by id desc \
          limit $7"
    ))
    .bind(organization_id)
    .bind(filter.client_id)
    .bind(filter.tool.as_deref())
    .bind(filter.status.as_deref())
    .bind(window)
    .bind(filter.before)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    // The cursor is the last row's id, and `None` only when the page came back short — a full
    // page may still be the end, and asking for one more page is how the screen finds out.
    let next_before = (rows.len() as i64 == limit).then(|| rows[rows.len() - 1].id);
    Ok(InvocationPage {
        invocations: rows,
        next_before,
    })
}

/// One row by id, scoped to a tenant.
pub async fn read(
    pool: &PgPool,
    organization_id: Uuid,
    id: i64,
) -> Result<Option<InvocationRow>> {
    let row = sqlx::query_as::<_, InvocationRow>(&format!(
        "select {COLUMNS} from mcp_invocations where organization_id = $1 and id = $2"
    ))
    .bind(organization_id)
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// The numbers the clients screen's header shows.
#[derive(Debug, Clone, Serialize)]
pub struct InvocationTotals {
    /// Calls in the window, every status.
    pub calls: i64,
    /// Calls that wrote something.
    pub ok: i64,
    /// Refused for a missing grant, a missing scope or a disabled tool.
    pub denied: i64,
    /// Calls that only proved a plan.
    pub sandbox: i64,
    /// Calls waiting on a human.
    pub pending_approval: i64,
    /// Calls the air gap stopped.
    pub blocked_airgap: i64,
    /// Calls that failed for any other reason.
    pub errors: i64,
    /// Mean duration over the window, or `None` when nothing was called.
    pub avg_duration_ms: Option<f64>,
}

/// Count the window.
///
/// Computed in SQL rather than by reading the page and counting it, because the page is capped at
/// 200 and a header that says "3 calls" for an installation that made 3 000 is worse than no
/// header. One grouped query with a `filter`, not one query per status.
pub async fn totals(
    pool: &PgPool,
    organization_id: Uuid,
    window_seconds: i64,
) -> Result<InvocationTotals> {
    let window = window_seconds.max(1);
    #[derive(sqlx::FromRow)]
    struct Row {
        calls: i64,
        ok: i64,
        denied: i64,
        sandbox: i64,
        pending_approval: i64,
        blocked_airgap: i64,
        errors: i64,
        avg_duration_ms: Option<f64>,
    }
    let row = sqlx::query_as::<_, Row>(
        "select count(*)::bigint as calls, \
                count(*) filter (where status = 'ok')::bigint as ok, \
                count(*) filter (where status = 'denied')::bigint as denied, \
                count(*) filter (where status = 'sandbox')::bigint as sandbox, \
                count(*) filter (where status = 'pending_approval')::bigint as pending_approval, \
                count(*) filter (where status = 'blocked_airgap')::bigint as blocked_airgap, \
                count(*) filter (where status = 'error')::bigint as errors, \
                avg(duration_ms)::float8 as avg_duration_ms \
           from mcp_invocations \
          where organization_id = $1 and created_at > now() - make_interval(secs => $2::bigint)",
    )
    .bind(organization_id)
    .bind(window)
    .fetch_one(pool)
    .await?;
    Ok(InvocationTotals {
        calls: row.calls,
        ok: row.ok,
        denied: row.denied,
        sandbox: row.sandbox,
        pending_approval: row.pending_approval,
        blocked_airgap: row.blocked_airgap,
        errors: row.errors,
        avg_duration_ms: row.avg_duration_ms,
    })
}

/// Per-tool counts, for the "which tool do they actually use" row on the client detail.
#[derive(Debug, Clone, Serialize)]
pub struct ToolUsage {
    pub tool: String,
    pub calls: i64,
    pub denied: i64,
}

/// The busiest tools in the window, most called first.
pub async fn usage_by_tool(
    pool: &PgPool,
    organization_id: Uuid,
    window_seconds: i64,
    limit: i64,
) -> Result<Vec<ToolUsage>> {
    let rows: Vec<(String, i64, i64)> = sqlx::query_as(
        "select tool, count(*)::bigint, \
                count(*) filter (where status = 'denied')::bigint \
           from mcp_invocations \
          where organization_id = $1 and created_at > now() - make_interval(secs => $2::bigint) \
          group by tool order by 2 desc, tool asc \
          limit $3",
    )
    .bind(organization_id)
    .bind(window_seconds.max(1))
    .bind(limit.clamp(1, 50))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(tool, calls, denied)| ToolUsage { tool, calls, denied })
        .collect())
}

/// Delete a client's invocations, for a client deleted on purpose.
///
/// Separate from `McpStore::delete_client` because the panel's delete route reads as "remove this
/// client and everything it did", and having the cascade be implicit is how an operator ends up
/// with a deleted client and no history without having been asked to choose. The count comes back
/// so the route can report what went with it rather than answering a bare 200.
pub async fn purge_client(pool: &PgPool, client_id: Uuid) -> Result<u64> {
    let done = sqlx::query("delete from mcp_invocations where client_id = $1")
        .bind(client_id)
        .execute(pool)
        .await?;
    Ok(done.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_vocabulary_covers_every_path_a_call_can_take() {
        // Every arm of `mcp_tools::Resolution` must map onto a status the store accepts, or a
        // refusal becomes a `500` from a check constraint. This is the test that keeps the two
        // vocabularies in step.
        for status in [
            crate::mcp_tools::Resolution::NotGranted.status(),
            crate::mcp_tools::Resolution::MissingPermission {
                permission: "content.pages.publish".into(),
            }
            .status(),
            crate::mcp_tools::Resolution::NeedsApproval {
                permission: "deployment.deploy".into(),
            }
            .status(),
            crate::mcp_tools::Resolution::Allow {
                permission: "content.pages.read".into(),
                approval_required: false,
            }
            .status(),
            crate::mcp_tools::Resolution::Disabled.status(),
        ] {
            assert!(
                STATUSES.contains(&status),
                "`{status}` is a resolution status the store would refuse"
            );
        }
    }

    #[test]
    fn a_sandbox_call_is_neither_a_success_nor_a_failure() {
        let mut row = sample("ok");
        assert!(row.succeeded());
        assert!(!row.is_pending());

        row.status = "sandbox".into();
        assert!(!row.succeeded(), "a sandbox proves a plan; it writes nothing");

        row.status = "pending_approval".into();
        assert!(row.is_pending());
        assert!(!row.succeeded());
    }

    fn sample(status: &str) -> InvocationRow {
        InvocationRow {
            id: 1,
            organization_id: Uuid::nil(),
            client_id: None,
            jsonrpc_id: None,
            tool: "content.search".into(),
            permission: Some("content.pages.read".into()),
            arguments_sha256: "abc".into(),
            arguments_preview: serde_json::json!({}),
            status: status.into(),
            error_code: None,
            duration_ms: 3,
            approval_id: None,
            run_id: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }
}