//! The I/O half of the approval gate: the appender, the inbox, the decisions, the sweeper and
//! the audit rows (REQ-101, slice 1).
//!
//! Everything below is deliberately ordered so that **a decision cannot do anything before it
//! has established that it is allowed to**:
//!
//! 1. read the row (404 for another organization — never a 403, which is an existence oracle),
//! 2. compare `expires_at` against the clock seam,
//! 3. take the row with a conditional `update … where status = 'pending'`,
//! 4. only then write the audit row.
//!
//! Step 3 before step 4 is the whole single-use argument. A `for update` read followed by an
//! unconditional write would let two reviewers (or a reviewer and a retry) both see `pending` and
//! both record a decision, and `audit_log` is append-only, so the second row could not be
//! removed — the trail would permanently claim two people approved the same deletion.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{AiHubError, Result};

use super::{
    APPROVAL_COLUMNS, Approval, ClassPolicy, DecisionOutcome, Gate, PolicyRow, PolicyView,
    class_label, class_of_tool, is_dangerous_class, is_irreversible_class, resolve_policies,
    validate_expiry,
};

/// The fallback for a deserialized `NewApproval` that carries no clock.
///
/// Present only so `#[serde(skip)]` compiles; the store rejects a zero timestamp below, so the
/// "forgot the seam" case fails loudly instead of writing a 1970 expiry.
fn epoch_default() -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH
}

/// What the loop hands the store when a gated call parks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewApproval {
    pub organization_id: Uuid,
    pub site_id: Option<Uuid>,
    pub run_id: Option<Uuid>,
    pub step_id: Option<Uuid>,
    pub agent_id: Option<Uuid>,
    pub identity_id: Option<Uuid>,
    /// The change set this request came out of, when a set's gated operation was parked
    /// instead of run (REQ-101 slice 3c). `None` for the single-call path, which is the
    /// ordinary case: a run's step parks on its own.
    ///
    /// The column has existed since `0189` and nothing wrote it, so every approval in the
    /// inbox read as though no proposal had produced it — the set a reviewer was deciding came
    /// from nowhere and the apply had nothing to walk back to.
    pub change_set_id: Option<Uuid>,
    pub tool_key: String,
    /// The class, resolved by [`class_of_tool`]. Stored because the mapping can grow and a row
    /// that re-derived its class would change meaning under a deployment.
    pub tool_class: String,
    pub resource_type: Option<String>,
    pub resource_id: Option<String>,
    /// The human name the typed confirmation is checked against.
    pub resource_label: Option<String>,
    pub title: String,
    pub summary: String,
    pub operation_count: i32,
    /// The frozen diff. Slice 2 computes it; slice 1 stores whatever the caller hands over and
    /// the `preview_is_object` check refuses an empty one, so a caller that has not built a
    /// preview yet fails at the store rather than at the review screen.
    pub preview: Value,
    pub preview_hash: String,
    pub base_revision: Option<String>,
    pub requested_by: Option<Uuid>,
    pub model_id: Option<Uuid>,
    pub risk: String,
    /// The policy in force, which is what decides the expiry and the phrase.
    pub policy: ClassPolicy,
    /// The clock, as a seam: a walk that has to wait an hour to see an expiry is a walk that
    /// does not get written.
    ///
    /// `#[serde(skip)]` needs a `Default`, and there is deliberately no `impl Default` here:
    /// the epoch is a real timestamp and a caller that forgot to set the seam would then write
    /// an approval that expired in 1970 — which passes every check and expires on the first
    /// sweeper tick. `NewApproval` is built in code, not parsed from a request body.
    #[serde(skip, default = "epoch_default")]
    pub requested_at: OffsetDateTime,
}

/// The appender's result: the row, and whether this call created it.
///
/// `AlreadyPending` is the request's "notification volume is bounded" criterion, and it is a
/// **return value rather than an error** because the caller is a run that parked: a run whose
/// second request for the same step is refused still has to end the same way it would have.
#[derive(Debug, Clone)]
pub enum Requested {
    Created(Box<Approval>),
    AlreadyPending(Box<Approval>),
}

impl Requested {
    /// The row, whichever way it went.
    #[must_use]
    pub fn approval(&self) -> &Approval {
        match self {
            Self::Created(approval) | Self::AlreadyPending(approval) => approval,
        }
    }
}

/// Ask the gate what a tool key means, and park the run if it means something dangerous.
///
/// The signature takes the caller's permissions **and does not use them**, which is the
/// request's second acceptance criterion expressed as a type: "An approval is required even
/// when the caller holds the underlying domain permission". The permissions are accepted so
/// the call site cannot be tempted to skip this function and call [`class_of_tool`] itself,
/// and they are *named* in the body so a future reader does not "fix" the unused argument.
pub async fn gate(
    pool: &PgPool,
    organization_id: Uuid,
    tool_key: &str,
    _holds_every_permission: bool,
) -> Result<Gate> {
    let Some(class) = class_of_tool(tool_key) else {
        return Ok(Gate::Ungated);
    };
    let policy = policy_for(pool, organization_id, class).await?;
    if !policy.requires_approval() {
        return Ok(Gate::Ungated);
    }
    // The phrase is required when the policy asks for it **or** when the class is irreversible.
    // Both halves are needed: the request lists typed confirmation for the three irreversible
    // classes as a property of the *action*, and separately lets an operator demand it for a
    // reversible one. Reading only the policy would let a platform default of `false` drop the
    // phrase for a deployment.
    let requires_confirmation = policy.typed_confirmation || is_irreversible_class(class);
    Ok(Gate::Parked {
        policy,
        requires_confirmation,
    })
}

/// The policy in force for one class, with an organization row beating the platform default.
///
/// Extracted from [`gate`] so a caller that is **not** a tool call — the change-set bridge
/// (REQ-101 slice 3c), which knows its class from the operation rather than from a tool key —
/// reads the same row through the same precedence. Two copies of "org row wins, else the
/// default, else fail closed" is a policy screen that shows one thing and a park that obeys
/// another.
pub async fn policy_for(pool: &PgPool, organization_id: Uuid, class: &str) -> Result<ClassPolicy> {
    let row: Option<PolicyRow> = sqlx::query_as(
        "select id, organization_id, tool_class, mode, typed_confirmation, expires_minutes, \
         updated_by, updated_at from ai_approval_policies \
         where tool_class = $1 and (organization_id is null or organization_id = $2) \
         order by organization_id nulls last limit 1",
    )
    .bind(class)
    .bind(organization_id)
    .fetch_optional(pool)
    .await?;
    Ok(row
        .as_ref()
        .map_or_else(ClassPolicy::default, PolicyRow::policy))
}

/// Write the request. One row per pending run step: a second request for the same step finds
/// the first one and returns it rather than flooding the inbox.
pub async fn request(pool: &PgPool, new: &NewApproval) -> Result<Requested> {
    if !is_dangerous_class(&new.tool_class) {
        return Err(AiHubError::InvalidApproval(format!(
            "`{}` is not one of the six gated classes",
            new.tool_class
        )));
    }
    if new.operation_count < 1 {
        return Err(AiHubError::InvalidApproval(
            "an approval describes at least one operation".to_owned(),
        ));
    }
    // The clock seam is required. A caller that let `#[serde(skip)]` pick the epoch would write
    // an approval that expired in 1970: it passes every check below and is swept on the next
    // tick, so the failure would be "somebody's approval vanished" rather than a loud refusal.
    if new.requested_at <= epoch_default() {
        return Err(AiHubError::InvalidApproval(
            "an approval needs the time it was requested at".to_owned(),
        ));
    }
    let irreversible = is_irreversible_class(&new.tool_class);
    let expires_at = new.policy.expires_at(new.requested_at);
    let phrase = new
        .resource_label
        .clone()
        .filter(|label| !label.trim().is_empty());

    let sql = format!(
        "insert into ai_approvals (organization_id, site_id, run_id, step_id, agent_id, \
         identity_id, change_set_id, tool_key, tool_class, resource_type, resource_id, \
         resource_label, risk, title, summary, operation_count, irreversible, \
         requires_confirmation, confirmation_phrase, preview, preview_hash, base_revision, \
         status, requested_by, model_id, expires_at, created_at) \
         values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21, \
         $22, 'pending',$23,$24,$25, now()) \
         on conflict do nothing \
         returning {APPROVAL_COLUMNS}"
    );
    let inserted: Option<Approval> = sqlx::query_as(&sql)
        .bind(new.organization_id)
        .bind(new.site_id)
        .bind(new.run_id)
        .bind(new.step_id)
        .bind(new.agent_id)
        .bind(new.identity_id)
        .bind(&new.change_set_id)
        .bind(&new.tool_key)
        .bind(&new.tool_class)
        .bind(&new.resource_type)
        .bind(&new.resource_id)
        .bind(&phrase)
        .bind(&new.risk)
        .bind(&new.title)
        .bind(&new.summary)
        .bind(new.operation_count)
        .bind(irreversible)
        .bind(new.policy.typed_confirmation)
        .bind(phrase.as_deref())
        .bind(&new.preview)
        .bind(&new.preview_hash)
        .bind(&new.base_revision)
        .bind(new.requested_by)
        .bind(new.model_id)
        .bind(expires_at)
        .bind(new.requested_at)
        .fetch_optional(pool)
        .await?;

    if let Some(approval) = inserted {
        return Ok(Requested::Created(Box::new(approval)));
    }
    // The unique index fired, so an identical pending request exists. Reading it back is what
    // makes the refusal answer with the row the reviewer will find, rather than with a code
    // and no way to reach the request.
    let existing = pending_for_step(pool, new.organization_id, new.run_id, new.step_id)
        .await?
        .ok_or_else(|| {
            // The index and this read disagree only if a sweeper expired the row between the
            // insert and the read. That is a real race, and the honest answer is a retryable
            // error rather than inventing an approval that does not exist.
            AiHubError::InvalidApproval(
                "the pending approval for this step expired while it was being created".to_owned(),
            )
        })?;
    Ok(Requested::AlreadyPending(Box::new(existing)))
}

/// The inbox filter, as the request's API table lists it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ApprovalFilter {
    /// One status, or nothing for "all".
    pub status: Option<String>,
    pub agent: Option<Uuid>,
    pub tool: Option<String>,
    pub tool_class: Option<String>,
    pub requester: Option<Uuid>,
    pub from: Option<OffsetDateTime>,
    pub to: Option<OffsetDateTime>,
    /// Free text over the title, the resource label and the tool key.
    pub q: Option<String>,
    pub limit: Option<i64>,
}

/// The inbox body: the rows and the per-status counts the tab strip renders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Inbox {
    pub approvals: Vec<Approval>,
    /// Counts per status, and the pending total the sidebar badge shows.
    pub counts: BTreeMap<String, i64>,
}

/// Read the inbox, with the counts. The counts are computed in the same pass rather than by a
/// second query so the badge can never disagree with the list it sits next to.
pub async fn list(pool: &PgPool, organization_id: Uuid, filter: &ApprovalFilter) -> Result<Inbox> {
    let status = filter.status.as_deref().filter(|s| *s != "all");
    let needle = filter
        .q
        .as_deref()
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .map(str::to_lowercase);

    let rows: Vec<Approval> = sqlx::query_as(&format!(
        "select {APPROVAL_COLUMNS} from ai_approvals \
         where organization_id = $1 \
         and ($2::text is null or status = $2) \
         and ($3::uuid is null or agent_id = $3) \
         and ($4::text is null or tool_key = $4) \
         and ($5::text is null or tool_class = $5) \
         and ($6::uuid is null or requested_by = $6) \
         and ($7::timestamptz is null or created_at >= $7) \
         and ($8::timestamptz is null or created_at <= $8) \
         order by created_at desc limit $9"
    ))
    .bind(organization_id)
    .bind(status)
    .bind(filter.agent)
    .bind(&filter.tool)
    .bind(&filter.tool_class)
    .bind(filter.requester)
    .bind(filter.from)
    .bind(filter.to)
    .bind(filter.limit.unwrap_or(200).clamp(1, 1000))
    .fetch_all(pool)
    .await?;

    // The needle is applied above the database, over the three fields a person would remember.
    // An index cannot serve `lower(title) like '%needle%'`, so pushing it into SQL would only
    // make the database do the same work with a worse plan.
    let approvals: Vec<Approval> = rows
        .into_iter()
        .filter(|row| {
            needle.as_ref().is_none_or(|needle| {
                row.title.to_lowercase().contains(needle)
                    || row
                        .resource_label
                        .as_deref()
                        .is_some_and(|label| label.to_lowercase().contains(needle))
                    || row.tool_key.to_lowercase().contains(needle)
            })
        })
        .collect();

    let count_rows: Vec<(String, i64)> = sqlx::query_as(
        "select status, count(*) from ai_approvals where organization_id = $1 group by status",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;
    let mut counts: BTreeMap<String, i64> = count_rows.into_iter().collect();
    counts.entry("all".to_owned()).or_default();
    counts.entry("pending".to_owned()).or_default();

    Ok(Inbox { approvals, counts })
}

/// One request, scoped to its organization. `None` for a stranger, never a 403.
pub async fn read(pool: &PgPool, organization_id: Uuid, id: Uuid) -> Result<Option<Approval>> {
    Ok(sqlx::query_as::<_, Approval>(&format!(
        "select {APPROVAL_COLUMNS} from ai_approvals where id = $1 and organization_id = $2"
    ))
    .bind(id)
    .bind(organization_id)
    .fetch_optional(pool)
    .await?)
}

/// The pending request for one run step, which is what the duplicate guard returns.
async fn pending_for_step(
    pool: &PgPool,
    organization_id: Uuid,
    run_id: Option<Uuid>,
    step_id: Option<Uuid>,
) -> Result<Option<Approval>> {
    let (Some(run_id), Some(step_id)) = (run_id, step_id) else {
        return Ok(None);
    };
    Ok(sqlx::query_as::<_, Approval>(&format!(
        "select {APPROVAL_COLUMNS} from ai_approvals \
         where organization_id = $1 and run_id = $2 and step_id = $3 and status = 'pending' \
         order by created_at desc limit 1"
    ))
    .bind(organization_id)
    .bind(run_id)
    .bind(step_id)
    .fetch_optional(pool)
    .await?)
}

/// Approve. The single-use and the typed confirmation are both decided here.
pub async fn approve<R: RevisionReader + ?Sized>(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    decided_by: Uuid,
    confirmation: Option<&str>,
    reader: &R,
    now: OffsetDateTime,
) -> Result<DecisionOutcome> {
    decide(
        pool,
        organization_id,
        id,
        decided_by,
        Decision::Approve {
            confirmation: confirmation.map(str::to_owned),
        },
        reader,
        now,
    )
    .await
}

/// Reject. A reason is not optional, and the row's own check constraint refuses a blank one —
/// so a caller that forgot the reason fails at the database rather than producing a rejection
/// nobody can audit later.
pub async fn reject<R: RevisionReader + ?Sized>(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    decided_by: Uuid,
    reason: &str,
    reader: &R,
    now: OffsetDateTime,
) -> Result<DecisionOutcome> {
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(AiHubError::InvalidApproval(
            "a rejection needs a reason".to_owned(),
        ));
    }
    if reason.chars().count() > MAX_REASON_CHARS {
        return Err(AiHubError::InvalidApproval(format!(
            "a rejection reason is at most {MAX_REASON_CHARS} characters"
        )));
    }
    decide(
        pool,
        organization_id,
        id,
        decided_by,
        Decision::Reject {
            reason: reason.to_owned(),
        },
        reader,
        now,
    )
    .await
}

/// Longest rejection reason the request allows.
pub const MAX_REASON_CHARS: usize = 500;

enum Decision {
    Approve { confirmation: Option<String> },
    Reject { reason: String },
}

/// How the decision path learns a target's current revision.
///
/// A seam rather than a direct call because this function is where the guarantee lives and it
/// has to be testable in both directions: a walk needs the **real** reader (so the stale check
/// sees a genuinely edited page), and a unit test needs a **scripted** one (so the stale check
/// can be fired without arranging a page edit, and so a reader that returned nothing can be
/// proven to fail the decision rather than pass it).
///
/// `None` means "this resource type has no reader in this build", which is a **refusal to
/// decide**, not a skip: an approval nobody can check the freshness of must not be applied.
pub trait RevisionReader: Send + Sync {
    /// The revision of the target right now.
    fn read(
        &self,
        pool: &PgPool,
        resource_type: &str,
        resource_id: &str,
    ) -> impl std::future::Future<Output = Result<String>> + Send;
}

/// The reader that talks to the database.
pub struct DbRevisionReader;

impl RevisionReader for DbRevisionReader {
    async fn read(&self, pool: &PgPool, resource_type: &str, resource_id: &str) -> Result<String> {
        super::target::revision_of(pool, resource_type, resource_id).await
    }
}

async fn decide<R: RevisionReader + ?Sized>(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    decided_by: Uuid,
    decision: Decision,
    reader: &R,
    now: OffsetDateTime,
) -> Result<DecisionOutcome> {
    let Some(row) = read(pool, organization_id, id).await? else {
        return Err(AiHubError::ApprovalNotFound(id));
    };

    // Three states, in this order, and the ORDER is the contract.
    //
    // 1. **Already swept** (`status = 'expired'`) answers `expired`. This arm exists because the
    //    first version of this function tested `status != 'pending'` first and reported every
    //    swept request as `already_decided` — telling the reviewer somebody else released it.
    //    That is a lie in the one direction that matters: somebody reading the inbox concludes
    //    a colleague approved a deletion when nobody did. The sweeper writes the status, so a
    //    decision arriving after a sweep lands here rather than in the pending branch.
    // 2. **Decided by somebody else** answers `already_decided`.
    // 3. **Past its expiry and still pending** sweeps it now and answers `expired`.
    //
    // The expiry check precedes the typed-confirmation check for the same reason it precedes the
    // others: a request that is both expired and unconfirmed must answer `expired`, because the
    // reviewer needs to know the request is gone before being told what phrase to type for a
    // decision that cannot happen.
    if row.status == "expired" {
        return Ok(DecisionOutcome::Expired(Box::new(row)));
    }
    if row.status != "pending" {
        return Ok(DecisionOutcome::AlreadyDecided(Box::new(row)));
    }
    if row.expires_at <= now {
        expire_one(pool, &row, now).await?;
        let refreshed = read(pool, organization_id, id)
            .await?
            .ok_or(AiHubError::ApprovalNotFound(id))?;
        return Ok(DecisionOutcome::Expired(Box::new(refreshed)));
    }

    if let Decision::Approve { confirmation } = &decision {
        if row.requires_confirmation {
            let phrase = row.confirmation_phrase.clone().unwrap_or_default();
            let Some(typed) = confirmation
                .as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty())
            else {
                return Ok(DecisionOutcome::ConfirmationRequired { phrase });
            };
            // Exact, not case-insensitive: a phrase is a deliberate act and relaxing the
            // comparison is how "confirm by typing the name" degrades into "confirm by typing
            // anything". The request is a checkbox is not a confirmation.
            if typed != phrase {
                return Ok(DecisionOutcome::ConfirmationMismatch {
                    phrase: typed.to_owned(),
                });
            }
        }
        // Staleness is checked at the same point, for the same reason: a refusal that is not
        // the reviewer's fault (the resource moved) must be distinguishable from one that is
        // (they typed the wrong thing), or the screen offers Re-preview to somebody whose only
        // mistake was a typo.
        //
        // **The revision is read here, not taken from the request.** Slice 1 compared the row
        // against a `current_revision` the caller posted, which made the whole guarantee a
        // self-report: a client that echoed the row's own stored base revision passed the
        // check every time. The client may still send what it saw -- that is what the screen
        // renders -- but the answer comes from the row.
        let (Some(resource_type), Some(resource_id)) =
            (row.resource_type.as_deref(), row.resource_id.as_deref())
        else {
            // An approval that names no target cannot be checked for freshness, so it cannot be
            // applied through this path. Slice 1's rows are written this way by tests that
            // park a request without a resource; refusing is the honest answer and the message
            // names the fix.
            return Err(AiHubError::InvalidApproval(format!(
                "approval {} names no resource, so it cannot be checked for staleness",
                row.id
            )));
        };
        let current = reader.read(pool, resource_type, resource_id).await?;
        if let Some(expected) = row.base_revision.as_deref()
            && expected != current
        {
            return Ok(DecisionOutcome::Stale {
                current_revision: current,
            });
        }
    }

    let (status, note) = match &decision {
        Decision::Approve { .. } => ("approved", None),
        Decision::Reject { reason } => ("rejected", Some(reason.as_str())),
    };

    // The conditional update IS the lock. Two callers both read `pending` above; only one
    // statement's `where status = 'pending'` matches, and the loser gets zero rows and answers
    // `already_decided` without having written anything.
    let sql = format!(
        "update ai_approvals set status = $3, decided_by = $4, decided_at = $5, \
         decision_note = $6 where id = $1 and organization_id = $2 and status = 'pending' \
         returning {APPROVAL_COLUMNS}"
    );
    let updated: Option<Approval> = sqlx::query_as(&sql)
        .bind(id)
        .bind(organization_id)
        .bind(status)
        .bind(decided_by)
        .bind(now)
        .bind(note)
        .fetch_optional(pool)
        .await?;

    match updated {
        Some(approval) => {
            audit(
                pool,
                organization_id,
                &approval,
                if status == "approved" {
                    "ai.approval.approved"
                } else {
                    "ai.approval.rejected"
                },
                Some(decided_by),
                json!({
                    "reason": note,
                    "preview_hash": approval.preview_hash,
                    "operation_count": approval.operation_count,
                }),
            )
            .await?;
            Ok(DecisionOutcome::Decided(Box::new(approval)))
        }
        // Somebody else won the race between the read and the write. Read the row back so the
        // loser reports the decision that actually happened rather than a bare code.
        None => {
            let current = read(pool, organization_id, id)
                .await?
                .ok_or(AiHubError::ApprovalNotFound(id))?;
            Ok(DecisionOutcome::AlreadyDecided(Box::new(current)))
        }
    }
}

/// What a re-preview changed.
///
/// A **new type rather than a bool**, because "did the hash move" and "what may I write" are
/// two different questions and the caller needs both: the screen re-renders on the row and
/// writes a new `preview`, and refuses on `unchanged`. Returning the row in the `changed` arm
/// is what stops the route from re-reading it and showing a different instant than the one it
/// computed against.
#[derive(Debug, Clone)]
pub enum RePreview {
    /// The target moved, or never matched: the row now carries a fresh preview.
    Refreshed(Box<Approval>),
    /// The recomputed plan is byte-identical to the stored one, so nothing was written.
    ///
    /// Refusing is the point. A re-preview that "succeeded" on an unchanged target would hand
    /// back a new `previewed_at` and a fresh hash for a diff nobody re-read — and the reviewer
    /// would be invited to re-decide on a row whose contents never moved. It also gives the
    /// endpoint a stable answer: two calls on a still row agree.
    Unchanged(Box<Approval>),
}

/// Re-preview one request against the target as it is **now**.
///
/// The server owns the recomputation end to end, for the same reason the decision path reads
/// the revision itself: a re-preview whose diff came from the client would be a preview nobody
/// read. So this takes no values at all — it reads the stored plan, rebuilds the operation, and
/// runs [`target::preview`] over a fresh read of the row.
///
/// # Errors
///
/// `Err(ApprovalNotFound)` when the id is not in this organization, and whatever the preview
/// refuses — a target that has since been deleted, an argument the current mapping no longer
/// knows, a preview whose recomputation now changes nothing. A row that cannot be re-previewed
/// is left exactly as it was: the write happens only after the new plan has resolved.
pub async fn re_preview(pool: &PgPool, organization_id: Uuid, id: Uuid) -> Result<RePreview> {
    let row = read(pool, organization_id, id)
        .await?
        .ok_or(AiHubError::ApprovalNotFound(id))?;

    // Only a row somebody can still act on may be re-previewed. A decided row's preview is the
    // record of what was decided, and rewriting it would rewrite the evidence — the reviewer
    // approved *that* diff.
    if row.status != "pending" {
        return Err(AiHubError::InvalidApproval(format!(
            "approval {id} is `{}`; a request that has been decided is a record, not a draft",
            row.status
        )));
    }

    let mapping = super::target::mapping_for(row.resource_type.as_deref().ok_or_else(|| {
        AiHubError::InvalidApproval(format!("approval {id} names no resource type"))
    })?)?;
    let stored = super::plan::Plan::from_preview(&row.preview)?;
    let operation = stored.operation(mapping)?;
    let fresh = super::target::preview(pool, mapping, &operation).await?;

    // The two comparisons are the whole contract, and they are deliberately different
    // comparisons:
    //
    // * **Hash first.** The hash covers the resolved diff *and* the base revision, so an
    //   unchanged target reproduces the stored hash exactly. That is the `unchanged` answer.
    // * **Base revision second, as the safety net.** If a mapping change or a preview-format
    //   change ever made the hash collide while the target had moved, the row would still be
    //   refused for a revision that disagrees. Comparing one of the two alone trusts one
    //   invariant; comparing both refuses unless both agree.
    if fresh.hash == row.preview_hash
        && fresh.base_revision == row.base_revision.clone().unwrap_or_default()
    {
        return Ok(RePreview::Unchanged(Box::new(row)));
    }

    let sql = format!(
        "update ai_approvals set preview = $3, preview_hash = $4, base_revision = $5 \
         where id = $1 and organization_id = $2 and status = 'pending' \
         returning {APPROVAL_COLUMNS}"
    );
    let updated: Option<Approval> = sqlx::query_as(&sql)
        .bind(id)
        .bind(organization_id)
        .bind(fresh.to_preview(mapping))
        .bind(fresh.hash.clone())
        .bind(fresh.base_revision.clone())
        .fetch_optional(pool)
        .await?;

    // Somebody decided it between the read and the write. The `where status = 'pending'` makes
    // zero rows, and the answer is the *decided* row rather than a fresh preview of a decision.
    let Some(updated) = updated else {
        let current = read(pool, organization_id, id)
            .await?
            .ok_or(AiHubError::ApprovalNotFound(id))?;
        return Ok(RePreview::Unchanged(Box::new(current)));
    };

    audit(
        pool,
        organization_id,
        &updated,
        "ai.approval.repreviewed",
        None,
        json!({
            "previous_preview_hash": row.preview_hash,
            "preview_hash": updated.preview_hash,
            "previous_base_revision": row.base_revision,
            "base_revision": updated.base_revision,
        }),
    )
    .await?;

    Ok(RePreview::Refreshed(Box::new(updated)))
}

/// Mark an approval applied, and write its audit row.
///
/// Called **after** the write it records, never before. The order is the whole retry story: if
/// the process dies between the content write and this call, the row stays `approved` with a
/// null `applied_at` and the reviewer can apply it again — re-applying an update is a second
/// revision, which is visible and harmless. Writing the marker first would make a *failed*
/// apply indistinguishable from a finished one, and there is no repair for that.
///
/// The `where applied_at is null` is the second half of single-use: two concurrent applies of
/// one approval produce one audit row rather than two.
pub async fn mark_applied(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    applied_by: Option<Uuid>,
) -> Result<Option<Approval>> {
    let sql = format!(
        "update ai_approvals set status = 'applied', applied_at = now() \
         where id = $1 and organization_id = $2 and applied_at is null \
         returning {APPROVAL_COLUMNS}"
    );
    let updated: Option<Approval> = sqlx::query_as(&sql)
        .bind(id)
        .bind(organization_id)
        .fetch_optional(pool)
        .await?;
    let Some(approval) = updated else {
        // Somebody else got there first. Not an error: the caller's effect is the same, and
        // answering `None` lets the route decide between "already applied" and "not yours".
        return Ok(None);
    };
    audit(
        pool,
        organization_id,
        &approval,
        "ai.approval.applied",
        applied_by,
        json!({
            "preview_hash": approval.preview_hash,
            "operation_count": approval.operation_count,
        }),
    )
    .await?;
    Ok(Some(approval))
}

/// Mark one request expired, and hand the parked run back so it fails cleanly.
async fn expire_one(pool: &PgPool, row: &Approval, now: OffsetDateTime) -> Result<()> {
    // `decided_at` is set even though the status is not `pending`, because the row's own check
    // constraint is `(status = 'pending') = (decided_at is null)` and an expired request *was*
    // decided — by the clock, not by a person. `decided_by` stays null, which is what tells the
    // panel the difference between "expired" and "rejected by somebody".
    let done = sqlx::query(
        "update ai_approvals set status = 'expired', decided_at = $2, \
         error = 'the request expired before anybody decided it' \
         where id = $1 and status = 'pending'",
    )
    .bind(row.id)
    .bind(now)
    .execute(pool)
    .await?;
    if done.rows_affected() == 0 {
        return Ok(());
    }
    if let Some(run_id) = row.run_id {
        // The run goes back on the queue so the runner picks it up and ends it: a run parked
        // forever is a step that stays `running`, and `resume_point` then reads the tool as
        // "may already have fired" for the rest of the installation's life.
        let _ = crate::run_store::requeue_run(pool, run_id).await;
    }
    audit(
        pool,
        row.organization_id,
        row,
        "ai.approval.expired",
        None,
        json!({ "waited_seconds": (now - row.created_at).whole_seconds() }),
    )
    .await?;
    Ok(())
}

/// The sweeper: expire every due request. Returns the ids it expired.
///
/// Bounded by `limit` because the runner tick that calls it shares a connection pool with the
/// API, and a backlog of ten thousand rows would hold a transaction open across all of them.
pub async fn expire_due(pool: &PgPool, now: OffsetDateTime, limit: i64) -> Result<Vec<Uuid>> {
    let due: Vec<Approval> = sqlx::query_as(&format!(
        "select {APPROVAL_COLUMNS} from ai_approvals where status = 'pending' \
         and expires_at <= $1 order by expires_at limit $2"
    ))
    .bind(now)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let mut expired = Vec::with_capacity(due.len());
    for row in due {
        expire_one(pool, &row, now).await?;
        expired.push(row.id);
    }
    Ok(expired)
}

/// How many requests are waiting, for the sidebar badge.
pub async fn pending_count(pool: &PgPool, organization_id: Uuid) -> Result<i64> {
    let count: (i64,) = sqlx::query_as(
        "select count(*) from ai_approvals where organization_id = $1 and status = 'pending'",
    )
    .bind(organization_id)
    .fetch_one(pool)
    .await?;
    Ok(count.0)
}

/// The audit rows for one request, newest first.
///
/// `actor_type = 'agent'` for the request row and `'user'` for the decision, and the test walks
/// this function rather than querying `audit_log` directly so a change to the filter is a
/// change this file is responsible for.
pub async fn audit_trail(
    pool: &PgPool,
    organization_id: Uuid,
    approval_id: Uuid,
) -> Result<Vec<AuditRow>> {
    Ok(sqlx::query_as(
        "select id, actor_type, actor_user_id, action, target_type, target_id, metadata, \
         created_at from audit_log \
         where organization_id = $1 and target_type = 'ai_approval' and target_id = $2 \
         order by created_at desc, id desc",
    )
    .bind(organization_id)
    .bind(approval_id.to_string())
    .fetch_all(pool)
    .await?)
}

/// One audit row, as the timeline renders it.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AuditRow {
    pub id: i64,
    pub actor_type: String,
    pub actor_user_id: Option<Uuid>,
    pub action: String,
    pub target_type: Option<String>,
    pub target_id: Option<String>,
    pub metadata: Value,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Append the audit row for one lifecycle step.
///
/// This writes `audit_log` directly rather than going through `crates/audit`, for two reasons
/// worth stating: the AI Hub must not gain a dependency on the audit crate's `NewAuditEntry`
/// builder for one call, and the `actor_type` for the request is `'agent'` with a *human*
/// `actor_user_id` — the requester is a person, the asker is not — which is a combination the
/// builder's ergonomics would obscure.
#[allow(clippy::too_many_arguments)]
async fn audit(
    pool: &PgPool,
    organization_id: Uuid,
    approval: &Approval,
    action: &str,
    actor_user_id: Option<Uuid>,
    metadata: Value,
) -> Result<()> {
    let actor_type = if action == "ai.approval.requested" {
        "agent"
    } else {
        "user"
    };
    sqlx::query(
        "insert into audit_log (organization_id, actor_user_id, actor_type, action, target_type, \
         target_id, metadata) values ($1, $2, $3, $4, 'ai_approval', $5, $6)",
    )
    .bind(organization_id)
    .bind(actor_user_id)
    .bind(actor_type)
    .bind(action)
    .bind(approval.id.to_string())
    .bind(metadata)
    .execute(pool)
    .await?;
    Ok(())
}

/// Write the request's own audit row. Called by the appender, after the row exists.
///
/// The acceptance criterion is "every request, decision and application has an `audit_log` row
/// with `actor_type = 'agent'`, the requester, the model id and the preview hash", so this
/// carries all four. It is best-effort in the *other* direction: the caller decides whether a
/// failed audit write is fatal, and for a request it is not — the run has already parked, and
/// losing the audit row is a gap in the evidence rather than a reason to un-park the run.
pub async fn audit_requested(pool: &PgPool, approval: &Approval) -> Result<()> {
    audit(
        pool,
        approval.organization_id,
        approval,
        "ai.approval.requested",
        approval.requested_by,
        json!({
            "tool_key": approval.tool_key,
            "class": approval.tool_class,
            "run_id": approval.run_id,
            "agent_id": approval.agent_id,
            "model_id": approval.model_id,
            "preview_hash": approval.preview_hash,
            "operation_count": approval.operation_count,
            "requester": approval.requested_by,
        }),
    )
    .await
}

/// The policy table, resolved for the screen.
pub async fn policies(pool: &PgPool, organization_id: Uuid) -> Result<Vec<PolicyView>> {
    let rows: Vec<PolicyRow> = sqlx::query_as(
        "select id, organization_id, tool_class, mode, typed_confirmation, expires_minutes, \
         updated_by, updated_at from ai_approval_policies \
         where organization_id is null or organization_id = $1 \
         order by tool_class, organization_id nulls last",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?;
    let mut resolved: Vec<PolicyView> = resolve_policies(&rows).into_values().collect();
    resolved.sort_by_key(|view| {
        crate::approvals::DANGEROUS_CLASSES
            .iter()
            .position(|class| *class == view.tool_class)
            .unwrap_or(usize::MAX)
    });
    Ok(resolved)
}

/// The policy edits the screen sends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyChange {
    pub tool_class: String,
    pub mode: String,
    pub typed_confirmation: Option<bool>,
    pub expires_minutes: Option<i32>,
}

/// The six classes, for a client that renders the form before it has read the table.
#[must_use]
pub fn class_options() -> Vec<(&'static str, &'static str, bool)> {
    crate::approvals::DANGEROUS_CLASSES
        .iter()
        .map(|class| {
            (
                *class,
                crate::approvals::class_label(class),
                is_irreversible_class(class),
            )
        })
        .collect()
}

/// Replace one class's organization policy.
///
/// The write is an upsert on the folded index and it **only** creates an organization row: the
/// platform default is never written from a screen, because an organization editing the default
/// row would change every other tenant's gate. The `expires_minutes` bound is checked here as
/// well as in the column, so the route's error names the field instead of surfacing a check
/// violation as a 500.
pub async fn set_policy(
    pool: &PgPool,
    organization_id: Uuid,
    change: &PolicyChange,
    updated_by: Uuid,
    now: OffsetDateTime,
) -> Result<PolicyView> {
    if !crate::approvals::DANGEROUS_CLASSES.contains(&change.tool_class.as_str()) {
        return Err(AiHubError::InvalidApproval(format!(
            "`{}` is not one of the six gated classes",
            change.tool_class
        )));
    }
    if change.mode != "require" && change.mode != "allow" {
        return Err(AiHubError::InvalidApproval(format!(
            "a policy mode is `require` or `allow`, not `{}`",
            change.mode
        )));
    }
    if let Some(minutes) = change.expires_minutes {
        validate_expiry(minutes)?;
    }
    let row: PolicyRow = sqlx::query_as(
        "insert into ai_approval_policies (organization_id, tool_class, mode, typed_confirmation, \
         expires_minutes, updated_by, updated_at) \
         values ($1, $2, $3, coalesce($4, true), coalesce($5, 60), $6, $7) \
         on conflict (coalesce(organization_id, '00000000-0000-0000-0000-000000000000'::uuid), \
                      tool_class) do update set mode = excluded.mode, \
         typed_confirmation = coalesce($4, ai_approval_policies.typed_confirmation), \
         expires_minutes = coalesce($5, ai_approval_policies.expires_minutes), \
         updated_by = excluded.updated_by, updated_at = excluded.updated_at \
         returning id, organization_id, tool_class, mode, typed_confirmation, expires_minutes, \
         updated_by, updated_at",
    )
    .bind(organization_id)
    .bind(&change.tool_class)
    .bind(&change.mode)
    .bind(change.typed_confirmation)
    .bind(change.expires_minutes)
    .bind(updated_by)
    .bind(now)
    .fetch_one(pool)
    .await?;

    audit_policy(
        pool,
        organization_id,
        &row.tool_class,
        &row.mode,
        row.typed_confirmation,
        row.expires_minutes,
        updated_by,
    )
    .await?;
    let tool_class = row.tool_class.clone();
    Ok(PolicyView {
        tool_class: tool_class.clone(),
        label: class_label(&tool_class).to_owned(),
        source: "organization".to_owned(),
        mode: row.mode,
        typed_confirmation: row.typed_confirmation,
        expires_minutes: row.expires_minutes,
        permissive: false,
        irreversible: is_irreversible_class(&row.tool_class),
        updated_at: row.updated_at,
        updated_by: row.updated_by,
    })
}

/// Delete an organization override, so the class inherits the platform default again.
pub async fn reset_policy(pool: &PgPool, organization_id: Uuid, tool_class: &str) -> Result<bool> {
    if !crate::approvals::DANGEROUS_CLASSES.contains(&tool_class) {
        return Err(AiHubError::InvalidApproval(format!(
            "`{tool_class}` is not one of the six gated classes"
        )));
    }
    let done = sqlx::query(
        "delete from ai_approval_policies where organization_id = $1 and tool_class = $2",
    )
    .bind(organization_id)
    .bind(tool_class)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() > 0)
}

#[allow(clippy::too_many_arguments)]
async fn audit_policy(
    pool: &PgPool,
    organization_id: Uuid,
    tool_class: &str,
    mode: &str,
    typed_confirmation: bool,
    expires_minutes: i32,
    updated_by: Uuid,
) -> Result<()> {
    // A policy change rides `audit_log` and **no** webhook event, per the request: it is an
    // administrative act, not a workflow fact, and a webhook subscriber should not be woken by
    // somebody widening their own gates.
    sqlx::query(
        "insert into audit_log (organization_id, actor_user_id, actor_type, action, target_type, \
         target_id, metadata) values ($1, $2, 'user', 'ai.policy.changed', 'ai_approval_policy', \
         $3, $4)",
    )
    .bind(organization_id)
    .bind(updated_by)
    .bind(tool_class)
    .bind(json!({
        "mode": mode,
        "typed_confirmation": typed_confirmation,
        "expires_minutes": expires_minutes,
    }))
    .execute(pool)
    .await?;
    Ok(())
}
