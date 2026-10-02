//! Moving a workflow between projects, with a dependency check that runs before anything moves
//! (REQ-133, slice 3).
//!
//! The REQ's promise is "a dependency resident outside the target project refuses the move" and
//! the interesting half is *which* dependencies exist. They are named there — credentials used,
//! sub-workflows, callers, schedules and triggers, inbound webhook subscriptions, published API
//! routes, workflow templates — and **most of them do not exist on this branch**. A `credentials`
//! table has never been created here; there is no sub-workflow action in the closed action set
//! (`noop`, `echo`, `fail`, `transient`, `send_email`, `comment_revision`); a schedule is a
//! column on `workflows`, not a table.
//!
//! **So the checker is written over the dependencies that exist, and it refuses to pretend about
//! the ones that do not.** Two things follow, and both are the design rather than a shortcut:
//!
//! * Every kind is a [`DependencyKind`] variant with a detector, and the set of *detected* kinds
//!   is a closed list in one place. A dependency that cannot be detected is a dependency the
//!   checker will silently pass over — which is the exact failure this function exists to prevent
//!   — so the kinds it cannot see are named in [`DependencyKind::UNSUPPORTED_ON_THIS_BRANCH`] and
//!   returned in every report, so the dry run tells an operator what it is *not* looking at
//!   instead of implying it checked everything.
//! * [`MoveReport::refuses`] is `true` when a dependency sits outside the target, and the move is
//!   a single `update … where` inside the same transaction as the read that produced the report.
//!   A dry run and a real move run the *same* code, so the report a dialog shows is the report
//!   the move was decided on.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{Result, WorkflowError};
use crate::projects::{self, ProjectCaller};

/// One thing that would follow a workflow into another project, or break there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Dependency {
    /// Execution history: the rows `workflow_executions.workflow_id` points at. These follow the
    /// workflow by cascade, so they are never a refusal — but the report counts them, because the
    /// REQ's second move criterion is "keeps the run history attached", and a count is how that
    /// is shown rather than asserted.
    RunHistory {
        /// How many execution rows would move with it.
        executions: i64,
    },
    /// Step rows of past runs (`workflow_steps`), the second half of the history.
    StepHistory {
        /// How many step rows would move with it.
        steps: i64,
    },
    /// A scheduled workflow's cursor. `schedule` and `next_run_at` live on the workflow row, so a
    /// move carries them; what the report shows is the next fire time, because a workflow that
    /// silently stops firing after a move is the classic way this operation goes wrong.
    ScheduleCursor {
        /// The cron expression being carried.
        schedule: String,
        /// When it next fires, as stored.
        next_run_at: Option<OffsetDateTime>,
    },
    /// An audit row recorded against this workflow in the *source* project. These do not move:
    /// `audit_log.project_id` is `on delete set null` and an audit trail that rewrites itself
    /// when a container is reorganised is not an audit trail. The report names them so an
    /// operator can see that history stays where it happened.
    SourceAuditHistory {
        /// How many rows stay behind.
        rows: i64,
    },
}

impl Dependency {
    /// The stable machine name, used by the report and by tests.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::RunHistory { .. } => "run_history",
            Self::StepHistory { .. } => "step_history",
            Self::ScheduleCursor { .. } => "schedule_cursor",
            Self::SourceAuditHistory { .. } => "source_audit_history",
        }
    }

    /// Whether this dependency makes the move unsafe.
    ///
    /// **True for none of the four kinds, on this branch.** That is not a claim that a move is
    /// free — it is what "there is no cross-project dependency kind that exists yet" means, and
    /// it is the sentence that makes the first one matter: when `credentials` ships with wave 7's
    /// runtime, the refusal path is already wired and the new kind only has to answer `true`.
    #[must_use]
    pub const fn refuses_move(&self) -> bool {
        match self {
            Self::RunHistory { .. }
            | Self::StepHistory { .. }
            | Self::ScheduleCursor { .. }
            | Self::SourceAuditHistory { .. } => false,
        }
    }
}

/// Everything the move dialog shows: what was found, what would break, and — the part that keeps
/// the promise honest — what this checker does not look at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveReport {
    /// The workflow being moved.
    pub workflow_id: Uuid,
    /// Its name, so a report can be read without a second request.
    pub workflow_name: String,
    /// Where it is now.
    pub from_project_id: Uuid,
    /// Where it is going.
    pub to_project_id: Uuid,
    /// Both keys, so the report names the projects rather than their ids.
    pub from_project_key: String,
    pub to_project_key: String,
    /// Everything found, in a stable order.
    pub dependencies: Vec<Dependency>,
    /// Dependency kinds the REQ names that **cannot exist on this branch yet**, and therefore are
    /// not checked. Empty the day they ship.
    ///
    /// Owned strings rather than `Vec<&'static str>`: a borrowed list forces
    /// `Deserialize` to borrow from the input document for the report's whole lifetime, which
    /// a `#[derive(Deserialize)]` cannot express — the response type outlives the response body.
    pub unchecked: Vec<String>,
    /// Whether the move is refused.
    pub refuses: bool,
    /// Why, when it is refused.
    pub reason: Option<String>,
    /// `true` when this was a dry run and nothing was written.
    pub dry_run: bool,
}

impl MoveReport {
    /// The dependency kinds that refuse a move, by name.
    #[must_use]
    pub fn blocking_kinds(&self) -> Vec<&'static str> {
        self.dependencies
            .iter()
            .filter(|dependency| dependency.refuses_move())
            .map(Dependency::kind)
            .collect()
    }
}

/// Dependency kinds named by REQ-133 that no table on this branch can hold.
///
/// This list is the reason the module is honest rather than quiet: a move dialog that reports
/// "no dependencies" when it checked nothing is worse than no dialog, and a report that carries
/// `unchecked` tells the operator exactly which guarantees are not being made on this branch.
pub const UNCHECKED_DEPENDENCY_KINDS: &[&str] = &[
    "credential_reference",
    "sub_workflow_call",
    "inbound_webhook_subscription",
    "published_api_route",
    "workflow_template",
    "workflow_folder",
];

/// Detect every dependency a workflow actually has, inside the caller's transaction.
///
/// Takes a connection rather than a pool because the caller is inside a transaction that is also
/// going to write the move, and a dry run wants the same read at the same isolation: a report
/// computed from a committed snapshot and a write issued afterwards is the check-then-write shape
/// the archive guard already had to be moved away from.
pub async fn detect_dependencies(
    connection: &mut sqlx::PgConnection,
    workflow_id: Uuid,
) -> Result<Vec<Dependency>> {
    let executions: i64 = sqlx::query_scalar(
        "select count(*) from workflow_executions where workflow_id = $1",
    )
    .bind(workflow_id)
    .fetch_one(&mut *connection)
    .await?;
    let steps: i64 = sqlx::query_scalar(
        "select count(*) from workflow_steps where execution_id in \
         (select id from workflow_executions where workflow_id = $1)",
    )
    .bind(workflow_id)
    .fetch_one(&mut *connection)
    .await?;
    let audit_rows: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where target_type = 'workflow' and target_id = $1::text",
    )
    .bind(workflow_id)
    .fetch_one(&mut *connection)
    .await?;

    let schedule: Option<(Option<String>, Option<OffsetDateTime>)> = sqlx::query_as(
        "select schedule, next_run_at from workflows where id = $1",
    )
    .bind(workflow_id)
    .fetch_optional(&mut *connection)
    .await?;

    let mut dependencies = Vec::new();
    if executions > 0 {
        dependencies.push(Dependency::RunHistory { executions });
    }
    if steps > 0 {
        dependencies.push(Dependency::StepHistory { steps });
    }
    if let Some((Some(cron), next_run_at)) = schedule {
        dependencies.push(Dependency::ScheduleCursor {
            schedule: cron,
            next_run_at,
        });
    }
    if audit_rows > 0 {
        dependencies.push(Dependency::SourceAuditHistory { rows: audit_rows });
    }
    Ok(dependencies)
}

/// Report on a move without writing anything.
pub async fn plan_move(
    pool: &PgPool,
    organization_id: Uuid,
    workflow_id: Uuid,
    to_project_id: Uuid,
    caller: ProjectCaller,
) -> Result<MoveReport> {
    let mut connection = pool.acquire().await?;
    let plan = plan_move_in(&mut connection, organization_id, workflow_id, to_project_id, caller).await?;
    Ok(plan)
}

/// Report on a move, on a caller's connection.
///
/// The caller must already have proved it may see the workflow ([`projects::can_see_workflow`])
/// and the target project. Doing either here would repeat slice 2's visibility rules in a third
/// place, and slice 2 exists because a rule copied into a new caller is a rule that will drift.
///
/// The caller is taken and not used because a move has **no permission exemption of its own**:
/// once the visibility checks pass, the same rules apply to everybody, which is what happened when
/// the first version exempted instance administrators from the source-archive guard. The
/// parameter stays so that the day a rule does distinguish a caller, the signature is already
/// the one the callers use — and the unnamed binding is the compiler saying so.
async fn plan_move_in(
    connection: &mut sqlx::PgConnection,
    organization_id: Uuid,
    workflow_id: Uuid,
    to_project_id: Uuid,
    _caller: ProjectCaller,
) -> Result<MoveReport> {
    let row: Option<(Uuid, String, Uuid, String)> = sqlx::query_as(
        "select w.id, w.name, w.project_id, p.key from workflows w \
         join automation_projects p on p.id = w.project_id \
         where w.id = $1 and w.organization_id = $2",
    )
    .bind(workflow_id)
    .bind(organization_id)
    .fetch_optional(&mut *connection)
    .await?;
    let (_source_id, name, from_project_id, from_key) = row.ok_or_else(|| {
        WorkflowError::invalid(
            "workflow_not_found",
            "no such workflow in this organization",
        )
    })?;

    let target: Option<(String, String)> = sqlx::query_as(
        "select key, status from automation_projects where id = $1 and organization_id = $2",
    )
    .bind(to_project_id)
    .bind(organization_id)
    .fetch_optional(&mut *connection)
    .await?;
    let (to_key, to_status) = target.ok_or_else(|| {
        WorkflowError::invalid(
            "project_not_found",
            "the target project is not visible in this organization",
        )
    })?;

    let dependencies = detect_dependencies(connection, workflow_id).await?;
    let blocking = dependencies
        .iter()
        .find(|dependency| dependency.refuses_move());

    // Moving into the project it is already in is refused rather than made a no-op success. A
    // silent `update` that changes zero rows and answers 200 is how a caller learns to trust a
    // report that says nothing happened.
    if from_project_id == to_project_id {
        return Err(WorkflowError::invalid(
            "already_in_project",
            format!("this workflow is already in {to_key}"),
        ));
    }

    // A move into an archived project would land a workflow somewhere the engine refuses to run
    // it (see `projects::ensure_run_allowed`), so it is refused at plan time and named.
    if to_status == "archived" {
        return Err(WorkflowError::invalid(
            "project_archived",
            format!("{to_key} is archived — restore it before moving a workflow into it"),
        ));
    }

    // An archived *source* is read-only by the same rule, and a move is a write.
    //
    // **No instance-admin exemption, deliberately.** The first version of this function had one,
    // and the gate caught it: `ensure_run_allowed` refuses runs in an archived project for
    // everybody, so an administrator could not start a run in an archived project but could
    // reorganise it. "Archived means read-only" is a property of the state, not a permission, and
    // the way out is `/restore` — which exists and is audited. Two guards with the same name and
    // different exemptions is how "archived" comes to mean two things.
    let source_status: Option<String> =
        sqlx::query_scalar("select status from automation_projects where id = $1")
            .bind(from_project_id)
            .fetch_optional(&mut *connection)
            .await?;
    if source_status.as_deref() == Some("archived") {
        return Err(WorkflowError::invalid(
            "project_archived",
            format!("{from_key} is archived — restore it before moving a workflow out of it"),
        ));
    }

    let reason = blocking.map(|dependency| match dependency {
        Dependency::RunHistory { executions } => format!(
            "run history ({executions} executions) is not movable on this branch"
        ),
        Dependency::StepHistory { steps } => {
            format!("step history ({steps} steps) is not movable on this branch")
        }
        Dependency::ScheduleCursor { schedule, .. } => {
            format!("the schedule \"{schedule}\" crosses a project boundary")
        }
        Dependency::SourceAuditHistory { rows } => {
            format!("{rows} audit rows belong to {from_key} and stay behind")
        }
    });

    Ok(MoveReport {
        workflow_id,
        workflow_name: name,
        from_project_id,
        to_project_id,
        from_project_key: from_key,
        to_project_key: to_key,
        refuses: blocking.is_some(),
        reason,
        dry_run: true,
        dependencies,
        unchecked: UNCHECKED_DEPENDENCY_KINDS
            .iter()
            .map(|kind| (*kind).to_string())
            .collect(),
    })
}

/// Move a workflow, transactionally, after the dependency check.
///
/// The check and the write are **one statement's worth of transaction**: `update … where id = $1
/// and project_id = $2` names the source project in the `where`, so a workflow that moved under
/// someone else between the report and this call updates zero rows and is reported as such rather
/// than as a success. `dry_run` runs the identical detection and returns before that statement, so
/// the report a dialog renders is produced by the code that decides the move.
pub async fn move_workflow(
    pool: &PgPool,
    organization_id: Uuid,
    workflow_id: Uuid,
    to_project_id: Uuid,
    actor_user_id: Option<Uuid>,
    dry_run: bool,
    caller: ProjectCaller,
) -> Result<MoveReport> {
    let mut tx = pool.begin().await?;

    let report = plan_move_in(
        &mut tx,
        organization_id,
        workflow_id,
        to_project_id,
        caller,
    )
    .await?;

    if dry_run {
        tx.rollback().await?;
        return Ok(report);
    }
    if report.refuses {
        tx.rollback().await?;
        return Err(WorkflowError::invalid(
            "move_blocked_by_dependency",
            report
                .reason
                .unwrap_or_else(|| "a dependency of this workflow lives outside the target project".into()),
        ));
    }

    let moved = sqlx::query(
        "update workflows set project_id = $2, updated_at = now() \
         where id = $1 and project_id = $3",
    )
    .bind(workflow_id)
    .bind(to_project_id)
    .bind(report.from_project_id)
    .execute(&mut *tx)
    .await?;

    if moved.rows_affected() == 0 {
        // The workflow moved between the report and this write. Reported as the conflict it is,
        // rather than as a success that changed nothing.
        tx.rollback().await?;
        return Err(WorkflowError::invalid(
            "workflow_moved",
            "this workflow moved to another project while the move was being prepared — reload and try again",
        ));
    }

    sqlx::query(
        "insert into audit_log (organization_id, actor_user_id, actor_type, action, target_type, \
         target_id, metadata, project_id) \
         values ($1, $2, 'user', 'workflow.moved', 'workflow', $3::text, $4, $5)",
    )
    .bind(organization_id)
    .bind(actor_user_id)
    .bind(workflow_id)
    .bind(serde_json::json!({
        "from_project_id": report.from_project_id,
        "from_project_key": report.from_project_key,
        "to_project_id": report.to_project_id,
        "to_project_key": report.to_project_key,
        "dependencies": report.dependencies.len(),
    }))
    .bind(to_project_id)
    .execute(&mut *tx)
    .await?;

    // Re-stamp the search index row, in this transaction.
    //
    // **`search_documents.project_id` is the scoping column for global search.** The clause in
    // `omnion_search::query::search` filters on the value stored on the document, not on the
    // workflow row, and the document is written by the search provider's upsert — which runs on a
    // reindex and on nothing else: `index_entity` has no caller and there is no periodic reindex.
    // So without this line the index keeps naming the project the workflow LEFT, and the failure is
    // not symmetric. The member who **gained** access cannot find it; the member who **lost** access
    // keeps finding it, for ever, because nothing re-stamps the row. A `?project=<id>` in a shared
    // link does not help — the clause reads the stored value.
    //
    // Why a plain `update` rather than a call into the search crate: `omnion-workflows` must not
    // grow a dependency on `omnion-search`, whose whole design is to take project ids as *values*
    // (see `omnion-search`'s Cargo.toml, which says so). The index row is keyed
    // `(provider, entity_type, entity_id)` and carries no foreign key to `workflows`, so stamping
    // one column is well defined — and it is `where provider = 'workflows'`, so a document for
    // some other entity that happens to share the id cannot be touched.
    //
    // **Touching zero rows is not an error.** An unindexed workflow has no document to update, and
    // refusing the move over one would mean a workflow could not be moved until somebody ran a
    // reindex. The stale-index problem is fixed at the source (the move); the missing-row case is
    // handled by the indexer when it eventually runs.
    //
    // Inside the transaction rather than after the commit, for the same reason the audit row is:
    // a stamp that survives a rolled-back move is a document scoped to a project the workflow is
    // not in — the leak this line exists to close, opened by the fix itself.
    sqlx::query(
        "update search_documents set project_id = $1, indexed_at = now() \
         where provider = 'workflows' and entity_type = 'workflow' and entity_id = $2::text",
    )
    .bind(to_project_id)
    .bind(workflow_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(MoveReport {
        dry_run: false,
        ..report
    })
}

/// Re-exported so a handler can prove visibility without reaching into [`projects`].
pub use projects::ProjectCaller as Caller;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_always_names_what_it_did_not_check() {
        // The whole reason `unchecked` is a field: a dependency kind that does not exist yet
        // cannot be detected, and a report that implied full coverage would be a lie the move
        // dialog tells with a straight face.
        assert!(UNCHECKED_DEPENDENCY_KINDS.contains(&"credential_reference"));
        assert!(UNCHECKED_DEPENDENCY_KINDS.contains(&"sub_workflow_call"));
        // …and the kinds it *does* check are not also on that list.
        for kind in [
            Dependency::RunHistory { executions: 1 }.kind(),
            Dependency::StepHistory { steps: 1 }.kind(),
            Dependency::SourceAuditHistory { rows: 1 }.kind(),
            Dependency::ScheduleCursor {
                schedule: "* * * * *".into(),
                next_run_at: None,
            }
            .kind(),
        ] {
            assert!(!UNCHECKED_DEPENDENCY_KINDS.contains(&kind));
        }
    }

    #[test]
    fn no_dependency_kind_refuses_a_move_on_this_branch() {
        // Recorded as a test rather than left as a comment, because the first kind that *does*
        // refuse has to change this and this is where it changes.
        for dependency in [
            Dependency::RunHistory { executions: 3 },
            Dependency::StepHistory { steps: 9 },
            Dependency::ScheduleCursor {
                schedule: "0 * * * *".into(),
                next_run_at: None,
            },
            Dependency::SourceAuditHistory { rows: 1 },
        ] {
            assert!(!dependency.refuses_move(), "{} should not refuse", dependency.kind());
        }
    }

    #[test]
    fn blocking_kinds_lists_only_the_refusing_ones() {
        let report = MoveReport {
            workflow_id: Uuid::nil(),
            workflow_name: "nightly".into(),
            from_project_id: Uuid::nil(),
            to_project_id: Uuid::from_u128(1),
            from_project_key: "OPS".into(),
            to_project_key: "BILLING".into(),
            dependencies: vec![
                Dependency::RunHistory { executions: 2 },
                Dependency::SourceAuditHistory { rows: 1 },
            ],
            unchecked: UNCHECKED_DEPENDENCY_KINDS.iter().map(|k| (*k).to_string()).collect(),
            refuses: false,
            reason: None,
            dry_run: true,
        };
        assert!(report.blocking_kinds().is_empty());
        assert!(!report.refuses);
        assert_eq!(report.dependencies.len(), 2);
    }

    #[test]
    fn a_report_serialises_its_kind_for_the_dialog() {
        let dependency = Dependency::ScheduleCursor {
            schedule: "*/5 * * * *".into(),
            next_run_at: None,
        };
        let json = serde_json::to_value(&dependency).expect("report serialises");
        assert_eq!(json["kind"], "schedule_cursor");
        assert_eq!(json["schedule"], "*/5 * * * *");
    }
}
