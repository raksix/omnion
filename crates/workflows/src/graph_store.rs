//! Reading and writing a workflow's graph, and the save that writes both representations.
//!
//! Deliberately *not* part of [`crate::model::WORKFLOW_COLUMNS`]: the engine's hot path — the
//! runner claiming a due step, the scheduler arming a schedule — has no reason to carry a
//! 200-node document, and a `jsonb` column on every definition read is a cost the automation
//! surface does not pay. The credential usage probe reached the same conclusion from the other
//! direction (`crate::credential_store` probes `graph` only where it must, and `steps` beside it
//! for installs that predate the column).
//!
//! The one thing this module insists on is that `graph` and `steps` are written **together**.
//! A save that wrote the graph and left `steps` for later would leave a run executing yesterday's
//! definition while the canvas shows today's — the exact drift the REQ calls out as its first
//! risk. So the update is one statement.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::definition::StepDefinition;
use crate::error::{Result, WorkflowError};
use crate::graph::Graph;

/// The stored graph of a workflow, plus the revision the editor loaded.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct StoredGraph {
    /// The workflow it belongs to.
    pub workflow_id: Uuid,
    /// Organization that owns it — the row carries the scope so a reader never has to join.
    pub organization_id: Uuid,
    /// The document.
    pub graph: serde_json::Value,
    /// The optimistic-concurrency counter.
    pub graph_revision: i32,
    /// When the canvas was last saved.
    pub graph_updated_at: Option<OffsetDateTime>,
    /// Who saved it last.
    pub graph_updated_by: Option<Uuid>,
}

/// Why a save was refused, in the shape the canvas's conflict banner reads.
#[derive(Debug, Clone, PartialEq)]
pub enum SaveOutcome {
    /// The graph was written; here is the new revision.
    Saved {
        /// The revision the row now carries.
        revision: i32,
    },
    /// Somebody else saved first. The row's current revision is here so the editor can offer
    /// compare-and-reload rather than a blind overwrite.
    Conflict {
        /// The revision the row carries now.
        current_revision: i32,
    },
    /// The workflow was deleted between the editor loading it and this save.
    ///
    /// A *variant* rather than an error because "it is gone" is not a failure of the store: the
    /// statement ran, the row it matched nothing, and the caller's correct response is a 404 it
    /// builds itself. An error here would have to be caught and re-classified by every caller to
    /// produce that same 404.
    Gone,
}

/// Read a workflow's graph and revision.
pub async fn find_graph(pool: &PgPool, workflow_id: Uuid) -> Result<Option<StoredGraph>> {
    let stored: Option<StoredGraph> = sqlx::query_as(
        "select id as workflow_id, organization_id, graph, graph_revision, graph_updated_at, \
         graph_updated_by from workflows where id = $1",
    )
    .bind(workflow_id)
    .fetch_optional(pool)
    .await?;

    Ok(stored)
}

/// Write the graph and its compiled steps in one statement, refusing a stale revision.
///
/// `expected_revision` is the revision the editor loaded. The `where` clause *is* the check: a
/// `select` followed by an `update` would leave a window in which two editors both read the same
/// revision and both write, and the loser's `⌘S` would appear to succeed. Letting the row lock
/// decide is the only version where "your canvas is out of date" is true when it is said.
pub async fn save_graph(
    pool: &PgPool,
    workflow_id: Uuid,
    graph: &Graph,
    steps: &[StepDefinition],
    expected_revision: i32,
    actor: Uuid,
) -> Result<SaveOutcome> {
    let steps_json = serde_json::to_value(steps).map_err(|err| {
        WorkflowError::invalid(
            "graph_compile_failed",
            format!("the compiled steps cannot be stored: {err}"),
        )
    })?;

    let updated: Option<(i32,)> = sqlx::query_as(
        "update workflows set graph = $2, steps = $3, graph_revision = graph_revision + 1, \
         graph_updated_at = now(), graph_updated_by = $4, updated_at = now() \
         where id = $1 and graph_revision = $5 \
         returning graph_revision",
    )
    .bind(workflow_id)
    .bind(graph.to_value())
    .bind(steps_json)
    .bind(actor)
    .bind(expected_revision)
    .fetch_optional(pool)
    .await?;

    if let Some((revision,)) = updated {
        return Ok(SaveOutcome::Saved { revision });
    }

    // No row matched. Either the workflow is gone or the revision moved — two different
    // sentences, so the caller is told which rather than being handed a bare conflict.
    let current: Option<(i32,)> =
        sqlx::query_as("select graph_revision from workflows where id = $1")
            .bind(workflow_id)
            .fetch_optional(pool)
            .await?;

    match current {
        Some((current_revision,)) => Ok(SaveOutcome::Conflict { current_revision }),
        None => Ok(SaveOutcome::Gone),
    }
}

/// The compiled steps a saved graph produced, or `None` when the workflow is gone.
///
/// The editor shows "these 5 nodes are these 5 steps"; this is where the mapping is read from
/// rather than recomputed, so a workflow saved by an older build (whose graph and steps were
/// written by different code) reports the mismatch rather than papering over it. A missing row
/// is `None` and not an error, so the API decides that a deleted workflow is a 404.
pub async fn steps_for(pool: &PgPool, workflow_id: Uuid) -> Result<Option<Vec<StepDefinition>>> {
    let raw: Option<(serde_json::Value,)> =
        sqlx::query_as("select steps from workflows where id = $1")
            .bind(workflow_id)
            .fetch_optional(pool)
            .await?;

    let Some((raw,)) = raw else {
        return Ok(None);
    };

    serde_json::from_value(raw).map(Some).map_err(|err| {
        WorkflowError::invalid(
            "workflow_definition_unreadable",
            format!("the stored steps are not readable: {err}"),
        )
    })
}
