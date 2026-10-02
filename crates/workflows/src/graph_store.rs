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

// `id as workflow_id`, not `workflow_id`: the `workflows` table's primary key is `id` (see
// `create table workflows`), and selecting `workflow_id` here fails at run time with
// 'column "workflow_id" does not exist'. The alias keeps the row tuple's shape — the
// GraphDefinition binds the first column to `workflow_id` — without renaming the column
// everywhere the query is written.
const GRAPH_COLUMNS: &str = "id as workflow_id, graph, ui_state, graph_version, validated_at, \
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
///
/// **`plugins` is the same registry the route validated against, and passing it is not
/// optional bookkeeping.** A `None` here would project with the core registry while the
/// caller validated with the organization's own, so the two could disagree about one graph —
/// and the disagreement is a save that is accepted and then refused, with a sentence
/// pointing at the wrong problem. `None` therefore means *no plugins* explicitly, not
/// "caller forgot".
/// Which SQL the projection outcome asks for.
///
/// A private helper because the choice of column is the whole fix and it is the part a reader
/// cannot check by looking at the call site: `steps = $3` and `steps = coalesce($3, steps)` differ
/// by one word and mean opposite things. On a graph that projects, both write the same list; on
/// one that does not, `$3` binds `NULL` and **erases the rule's last good definition**, which is
/// the difference between "not yet runnable" and "has never run" on the list screen.
fn step_column(steps: &Option<Value>) -> &'static str {
    if steps.is_some() {
        "steps = $3"
    } else {
        "steps = coalesce($3, steps)"
    }
}

pub async fn replace_graph(
    pool: &PgPool,
    workflow_id: Uuid,
    update: GraphUpdate,
    plugins: Option<&crate::plugin_nodes::PluginRegistry>,
) -> Result<Option<GraphDefinition>> {
    // **A SAVE IS NOT A RUN, AND A RULE IS BUILT BY BEING INCOMPLETE.** A graph is edited one
    // node at a time, so the author's first save is a rule whose cards are not wired together
    // yet — and refusing that write is refusing the first keystroke of the feature. What makes
    // this safe is that the projection is the only thing that can fail, and the runner refuses
    // a graph it cannot project anyway: the guard moved from "you may not save" to "you may not
    // run", where it was always the true statement.
    //
    // The step list is therefore left alone on a graph that does not project, rather than
    // blanked. The previous definition is what a rule ran last, and the row is what the list
    // screen shows; replacing it with an empty list would make "not yet runnable" indistinguishable
    // from "has never run" — the same conflation the trace panel was fixed for.
    let (steps, validation_error) = match project_steps(&update.graph, plugins) {
        Ok(steps) => (Some(steps), None),
        Err(error) => (None, Some(error.to_string())),
    };

    let step_column = step_column(&steps);
    let sql = format!(
        "update workflows \
            set graph = $2, \
                {step_column}, \
                ui_state = coalesce($4, ui_state), \
                graph_version = graph_version + 1, \
                validated_at = now(), \
                validation_error = $6, \
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
        .bind(validation_error)
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

/// Attribute every step of a run to the node of the graph it came from.
///
/// The canvas paints run status per node, and *Retry this node* addresses a run's steps by
/// the node they came from. Both read this one column, and a run whose steps carry no node
/// id has **no status layer at all** — not a partial one: the pills are empty, the click
/// opens nothing, and the retry answers "this node took no part in this run" on every
/// card. So the attribution is not a nicety of the *Run from here* feature that happens to
/// be useful elsewhere; it is what makes a run readable by node at all, and it belongs to
/// **every** path that starts a run.
///
/// It is a separate function rather than part of `create_execution` because the mapping is
/// `step_no → node_id`, and the two are produced by different code: the store materialises
/// steps from the *definition*, the walk resolves them against the *graph*. Doing the join
/// here means both run paths call the same join, and a third one added later gets it by
/// calling this.
///
/// A graph that does not project cleanly yields **no attribution at all** rather than a
/// partial one. A half-attributed run paints half its cards and leaves the rest blank,
/// which reads as "those nodes were skipped" — a claim about work the engine actually did.
/// An unrunnable graph is a validation problem the overview already reports.
pub async fn attribute_steps_to_graph(
    pool: &PgPool,
    execution_id: Uuid,
    graph: &crate::graph::Graph,
) -> Result<u64> {
    let walk = crate::graph::project_walk(graph)?;

    let mut attributed = 0_u64;
    for walked in &walk.nodes {
        let Some(step_no) = walked.step_no else {
            continue;
        };
        let updated = sqlx::query(
            "update workflow_steps set node_id = $3 where execution_id = $1::uuid \
               and step_no = $2 and node_id is null",
        )
        .bind(execution_id)
        .bind(step_no)
        .bind(&walked.node_id)
        .execute(pool)
        .await?
        .rows_affected();
        attributed += updated;
    }

    Ok(attributed)
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
fn project_steps(
    graph: &Graph,
    plugins: Option<&crate::plugin_nodes::PluginRegistry>,
) -> Result<Value> {
    let projected = match plugins {
        Some(registry) => crate::graph::project_with_plugins(graph, registry)?,
        None => crate::graph::project(graph)?,
    };
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

    /// **A RULE IS BUILT BY BEING INCOMPLETE, SO A SAVE MAY NOT REFUSE THE WORK.**
    ///
    /// The shape under test is a graph whose second card has no wire yet — the state every
    /// rule passes through on the way to being a rule. `project_steps` cannot walk it (it
    /// projects from the trigger and this graph's action is not reachable), and the old code
    /// carried that `?` straight out of the save, so the author's first edit was a `400` on a
    /// feature whose entire job is being edited.
    ///
    /// The negative control is the point: `some_steps`/`some_error` are what the save derives,
    /// so this test cannot pass against a server that writes an empty step list on a graph it
    /// cannot project — that would make "not yet runnable" look like "has never run", which is
    /// the one thing the trace panel was fixed for.
    #[test]
    fn a_save_of_a_graph_that_cannot_project_still_writes_the_definition() {
        let unwired = Graph {
            nodes: vec![node("trigger", "trigger.manual"), action("a1")],
            // No edge at all: the action exists and is not reachable, so the walk stops at the
            // trigger and the graph projects to nothing.
            edges: vec![],
        };

        let outcome = project_steps(&unwired, None);
        assert!(
            outcome.is_err(),
            "the fixture must be a graph that cannot project, or this test proves nothing",
        );

        // What `replace_graph` binds: the graph is written either way, the step list is left
        // alone when there is nothing to project, and the reason is recorded for the panel.
        let (steps, validation_error) = match project_steps(&unwired, None) {
            Ok(steps) => (Some(steps), None),
            Err(error) => (None, Some(error.to_string())),
        };
        assert!(
            steps.is_none(),
            "a graph that does not project has no step list to write"
        );
        let reason = validation_error.expect("the author is told why it cannot run yet");
        assert!(
            !reason.is_empty(),
            "a recorded reason that is empty reads on the list screen as a rule that is fine",
        );

        // **The half that actually reaches the database.** The previous version of this test
        // stopped one line earlier and passed against a server that blanked the step list on
        // a graph it could not project — the regression it was written for, silently green,
        // because nothing in it ever looked at the SQL. `steps = $3` binds NULL and erases the
        // rule's last good definition, which is how "not yet runnable" becomes "has never run"
        // on the list screen. This is the assertion that notices.
        assert_eq!(
            step_column(&steps),
            "steps = coalesce($3, steps)",
            "a graph that does not project must leave the previous step list alone, or the \
             rule's last runnable definition is erased by an edit that was not finished",
        );
    }

    /// The other direction, and the one the fix must not break: a graph that **does** project
    /// writes its steps and clears the recorded error. Without this the `coalesce` could keep a
    /// stale error forever and the list would call a working rule invalid.
    #[test]
    fn a_save_of_a_graph_that_projects_writes_its_steps_and_clears_the_error() {
        let (steps, validation_error) = match project_steps(&linear(), None) {
            Ok(steps) => (Some(steps), None),
            Err(error) => (None, Some(error.to_string())),
        };
        let array = steps
            .as_ref()
            .expect("a linear graph projects")
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            array.len(),
            2,
            "the projected list is what the runner executes"
        );
        assert!(
            validation_error.is_none(),
            "a projectable graph must not carry a validation error, or the list calls it invalid",
        );
        assert_eq!(
            step_column(&steps),
            "steps = $3",
            "a projectable graph writes its own step list; coalescing it would silently keep \
             the previous one when a node is removed from the middle of a rule",
        );
    }

    #[test]
    fn a_graph_projects_to_the_step_json_the_runner_reads() {
        let steps = project_steps(&linear(), None).expect("a linear graph projects");
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
        let error = project_steps(&graph, None).expect_err("a cycle does not project");
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
        assert!(project_steps(&graph, None).is_err());
    }
}
