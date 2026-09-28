//! Reading and writing the graph half of a definition (REQ-004 slice 1).
//!
//! The interesting write here is [`replace_graph`], and it exists for one reason: two people
//! can have the same rule open. Autosave is a timer, not a guarantee — the guarantee is the
//! version check. A save whose `graph_version` is not the stored one is refused with the
//! current definition, and the panel offers Reload while keeping the local copy on screen.
//! Silently overwriting would be the one behaviour an editor must never have.
//!
//! Two rules the SQL here holds to:
//!
//! * a **layout-only** save does not bump `graph_version` (REQ-004: "positions are not
//!   semantics"). A builder that panned and zoomed all afternoon must not have invalidated
//!   somebody else's edit, and `graph_version` is what a run pins.
//! * a save re-projects the step list in the **same statement**, so a rule cannot exist with
//!   a graph the runner is not executing.

use serde_json::Value;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{Result, WorkflowError};
use crate::graph::Graph;

/// How many step definitions the projection may produce.
const MAX_PROJECTED_STEPS: usize = crate::definition::MAX_STEPS;

/// The graph half of a definition: what the builder edits, at one version.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphDefinition {
    /// Workflow the graph belongs to.
    pub workflow_id: Uuid,
    /// The nodes and the connections between them.
    pub graph: Graph,
    /// Positions, viewport and collapsed groups — never read by the engine.
    pub ui_state: Value,
    /// The optimistic-concurrency token a `PUT` must quote.
    pub graph_version: i32,
    /// When the definition was last validated.
    pub validated_at: Option<OffsetDateTime>,
    /// The first validation error, when the last validation found one.
    pub validation_error: Option<String>,
    /// The projected step list the runner will execute.
    pub steps: Value,
}

/// A write to the graph half of a definition.
#[derive(Debug, Clone)]
pub struct GraphUpdate {
    /// The graph as posted.
    pub graph: Graph,
    /// Layout state as posted; `None` leaves the stored layout alone.
    pub ui_state: Option<Value>,
    /// The version the caller believes it is editing. A mismatch is a conflict, not a write.
    pub graph_version: i32,
}

const GRAPH_COLUMNS: &str = "workflow_id, graph, ui_state, graph_version, validated_at, \
     validation_error, steps";

/// Read the graph of one workflow.
///
/// `None` for a rule that is not there or belongs to another organization: the caller's
/// scope check is the tenancy gate, and a graph is part of the definition it guards.
pub async fn find_graph(pool: &PgPool, workflow_id: Uuid) -> Result<Option<GraphDefinition>> {
    let sql = format!("select {GRAPH_COLUMNS} from workflows where id = $1");
    let row: Option<(
        Uuid,
        Value,
        Value,
        i32,
        Option<OffsetDateTime>,
        Option<String>,
        Value,
    )> = sqlx::query_as(&sql)
        .bind(workflow_id)
        .fetch_optional(pool)
        .await?;

    Ok(row.map(
        |(workflow_id, graph, ui_state, graph_version, validated_at, validation_error, steps)| {
            // A row whose `graph` is not the object shape is read as an empty graph rather
            // than as a decode failure: the constraint prevents it, and a decode panic on a
            // read path would take out the list, not just the rule.
            GraphDefinition {
                workflow_id,
                graph: serde_json::from_value(graph).unwrap_or_default(),
                ui_state,
                graph_version,
                validated_at,
                validation_error,
                steps,
            }
        },
    ))
}

/// Replace a workflow's graph, its layout, and the step list the projection derives.
///
/// Returns the definition as stored, or `None` when the rule is gone. A `graph_version` that
/// is not the stored one is [`WorkflowError::Invalid`] with code `graph_version_conflict`
/// and the current definition's version in the message, so the client can show "Reload"
/// rather than guess.
pub async fn replace_graph(
    pool: &PgPool,
    workflow_id: Uuid,
    update: GraphUpdate,
) -> Result<Option<GraphDefinition>> {
    let steps = project_steps(&update.graph)?;

    let sql = format!(
        "update workflows \
            set graph = $2, \
                steps = $3, \
                ui_state = coalesce($4, ui_state), \
                graph_version = graph_version + 1, \
                validated_at = now(), \
                validation_error = null, \
                updated_at = now() \
          where id = $1 and graph_version = $5 \
          returning {GRAPH_COLUMNS}"
    );

    let stored: Option<(
        Uuid,
        Value,
        Value,
        i32,
        Option<OffsetDateTime>,
        Option<String>,
        Value,
    )> = sqlx::query_as(&sql)
        .bind(workflow_id)
        .bind(serde_json::to_value(&update.graph).unwrap_or(Value::Null))
        .bind(steps)
        .bind(update.ui_state)
        .bind(update.graph_version)
        .fetch_optional(pool)
        .await?;

    if stored.is_none() {
        // Either the rule is gone or somebody else wrote first. Both are the client's problem
        // to resolve by reloading, and the difference matters: a missing rule must not be
        // reported as a conflict that a reload can fix.
        let exists: Option<(Uuid, i32)> =
            sqlx::query_as("select id, graph_version from workflows where id = $1")
                .bind(workflow_id)
                .fetch_optional(pool)
                .await?;
        return match exists {
            None => Ok(None),
            Some((_, current)) => Err(WorkflowError::invalid(
                "graph_version_conflict",
                format!(
                    "this rule was saved by somebody else (it is now at version {current}); \
                     reload to see their change, or keep editing to overwrite it"
                ),
            )),
        };
    }

    let (workflow_id, graph, ui_state, graph_version, validated_at, validation_error, steps) =
        stored.expect("the update either returned a row or returned above");
    Ok(Some(GraphDefinition {
        workflow_id,
        graph: serde_json::from_value(graph).unwrap_or_default(),
        ui_state,
        graph_version,
        validated_at,
        validation_error,
        steps,
    }))
}

/// Write only the layout, leaving the version and the step list alone.
///
/// The autosave that fires while an author pans the canvas calls this, and it is the reason
/// `ui_state` is a separate column at all: a layout write that bumped `graph_version` would
/// make an afternoon of zooming invalidate a colleague's edit.
pub async fn replace_ui_state(pool: &PgPool, workflow_id: Uuid, ui_state: Value) -> Result<bool> {
    let updated = sqlx::query(
        "update workflows set ui_state = $2, updated_at = now() \
          where id = $1 and jsonb_typeof(ui_state) = 'object'",
    )
    .bind(workflow_id)
    .bind(ui_state)
    .execute(pool)
    .await?
    .rows_affected();

    Ok(updated > 0)
}

/// Record a validation verdict, so the rule list can draw "invalid" without re-validating.
///
/// `error` is the first finding's message, or `None` for a clean graph. A warning is not
/// stored: the panel's chip is about "this cannot run", not "this could be tidier".
pub async fn record_validation(
    pool: &PgPool,
    workflow_id: Uuid,
    error: Option<&str>,
) -> Result<()> {
    sqlx::query("update workflows set validated_at = now(), validation_error = $2 where id = $1")
        .bind(workflow_id)
        .bind(error)
        .execute(pool)
        .await?;
    Ok(())
}

/// Pin the graph a run started with, so a trace resolves its node ids years later.
pub async fn pin_execution_graph(
    pool: &PgPool,
    execution_id: Uuid,
    graph_version: Option<i32>,
) -> Result<()> {
    sqlx::query("update workflow_executions set graph_version = $2 where id = $1")
        .bind(execution_id)
        .bind(graph_version)
        .execute(pool)
        .await?;
    Ok(())
}

/// Mark which node of the graph a step came from, and which port carried into it.
///
/// The canvas paints run status per node from these rows, so a step written before the
/// builder existed simply carries no node id and paints nothing — which is honest, rather
/// than attributing the step to whatever node happens to sit at the same index.
pub async fn set_step_node(
    pool: &PgPool,
    step_id: Uuid,
    node_id: Option<&str>,
    branch: Option<&str>,
) -> Result<()> {
    sqlx::query("update workflow_steps set node_id = $2, branch = $3 where id = $1")
        .bind(step_id)
        .bind(node_id)
        .bind(branch)
        .execute(pool)
        .await?;
    Ok(())
}

/// The step list a graph projects onto, as the stored JSON.
///
/// A private helper because the projection must be reached from exactly one place per save:
/// two call sites would be two ways for a rule's graph and its steps to disagree.
fn project_steps(graph: &Graph) -> Result<Value> {
    let projected = crate::graph::project(graph)?;
    if projected.len() > MAX_PROJECTED_STEPS {
        return Err(WorkflowError::invalid(
            "too_many_steps",
            format!(
                "the graph projects to {} steps and a definition holds at most {MAX_PROJECTED_STEPS}",
                projected.len()
            ),
        ));
    }
    let steps: Vec<Value> = projected
        .iter()
        .map(|(_, step)| {
            serde_json::to_value(step).map_err(|error| {
                WorkflowError::invalid(
                    "graph_projection_failed",
                    format!("a step could not be written back as JSON: {error}"),
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;

    serde_json::to_value(steps).map_err(|error| {
        WorkflowError::invalid(
            "graph_projection_failed",
            format!("the projected steps could not be written as JSON: {error}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::graph::{Edge, Node, Position};

    fn node(id: &str, node_type: &str) -> Node {
        Node {
            id: id.to_owned(),
            node_type: node_type.to_owned(),
            label: id.to_owned(),
            params: json!({}),
            position: Position::default(),
        }
    }

    fn action(id: &str) -> Node {
        let mut node = node(id, "action");
        node.params = json!({ "action": "echo", "parameters": { "value": 1 } });
        node
    }

    fn linear() -> Graph {
        Graph {
            nodes: vec![
                node("trigger", "trigger.manual"),
                action("a1"),
                node("end", "end"),
            ],
            edges: vec![
                Edge {
                    id: "e0".to_owned(),
                    source: "trigger".to_owned(),
                    source_port: "out".to_owned(),
                    target: "a1".to_owned(),
                },
                Edge {
                    id: "e1".to_owned(),
                    source: "a1".to_owned(),
                    source_port: "success".to_owned(),
                    target: "end".to_owned(),
                },
            ],
        }
    }

    #[test]
    fn a_graph_projects_to_the_step_json_the_runner_reads() {
        let steps = project_steps(&linear()).expect("a linear graph projects");
        let array = steps.as_array().expect("steps are a JSON array");
        assert_eq!(array.len(), 2);
        // The same shape `update_workflow` writes for the linear editor — the engine reads
        // one or the other and must not be able to tell which produced it.
        assert_eq!(array[0]["name"], "a1");
        assert_eq!(array[0]["kind"], "task");
        assert_eq!(array[0]["action"], "echo");
        assert_eq!(array[1]["kind"], "stop");
    }

    #[test]
    fn a_projection_is_refused_before_it_can_be_written() {
        // A graph the engine could not execute is never stored, so a rule cannot exist with a
        // canvas that says one thing and a run that does another.
        let mut graph = linear();
        graph.edges.push(Edge {
            id: "e2".to_owned(),
            source: "end".to_owned(),
            source_port: "default".to_owned(),
            target: "a1".to_owned(),
        });
        let error = project_steps(&graph).expect_err("a cycle does not project");
        assert_eq!(error.code(), "graph_invalid");
    }

    #[test]
    fn a_projection_is_never_empty() {
        // A definition whose graph projects to nothing would run zero steps and report
        // success — the worst possible answer, so it is refused at write time.
        let graph = Graph {
            nodes: vec![node("trigger", "trigger.manual")],
            edges: Vec::new(),
        };
        assert!(project_steps(&graph).is_err());
    }
}
