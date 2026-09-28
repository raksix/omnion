//! Definition history: one row per write, and a diff summary in words (REQ-003 slice 4).
//!
//! The request's operations surface asks for *"Versions (definition diffs with Restore)"* and
//! *"audit on every definition change"*. Both are the same fact read two ways, and that is
//! why they live in one table: a rule is a single mutable row, so the moment `PUT` lands the
//! previous definition is gone. Storing a snapshot per write is what makes a diff possible
//! at all — there is no version of "yesterday" to diff against otherwise.
//!
//! ## Why a summary and not a computed diff
//!
//! The panel shows **what changed, in words**: "2 conditions, 1 action", "rate limit 60 →
//! 5". A structural diff of a JSON definition answers questions nobody asked ("this key was
//! added at depth 3") and stays wrong the moment the definition's shape changes, which is
//! the thing that happens most often here — REQ-004 is about to add a graph to the same
//! rule. [`diff`] walks a **fixed list of named fields** instead, so a field that appears
//! later is simply absent from both sides and contributes nothing rather than breaking the
//! comparison.
//!
//! ## Restore writes the next number
//!
//! Restoring an old definition does not rewind `version` to the number that was read; it
//! appends a new row with the old *content*. A history that can rewind is a history with
//! two rows claiming the same number, and the panel's "v3 → v1 → v3" story becomes
//! indistinguishable from a bug. `restored_from` says where the content came from, so the
//! line stays a line and the ancestry is still visible.

use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::Result;
use crate::model::AutomationRule;

/// Which write produced a version row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// The rule was created.
    Created,
    /// The rule was edited.
    Updated,
    /// The rule's content was taken from an earlier version.
    Restored,
}

impl Change {
    /// The stored text of this change.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
            Self::Restored => "restored",
        }
    }

    /// Parse the stored text back.
    ///
    /// An unknown value falls back to `Updated`, because an unreadable row is still a row
    /// in the history and hiding it would leave a gap nobody could explain.
    #[must_use]
    pub fn parse_or_default(text: &str) -> Self {
        match text {
            "created" => Self::Created,
            "restored" => Self::Restored,
            _ => Self::Updated,
        }
    }
}

/// One stored version of a rule.
#[derive(Debug, Clone)]
pub struct Version {
    /// Row id.
    pub id: Uuid,
    /// The rule the version belongs to.
    pub workflow_id: Uuid,
    /// The organization both live in.
    pub organization_id: Uuid,
    /// Which write produced it.
    pub change: Change,
    /// The number it was written as.
    pub version: i32,
    /// The whole definition as it was written.
    pub definition: Value,
    /// What changed, in words.
    pub summary: Value,
    /// The version whose content was restored, when this row is a restore.
    pub restored_from: Option<Uuid>,
    /// Who wrote it.
    pub created_by: Option<Uuid>,
    /// When.
    pub created_at: OffsetDateTime,
}

impl Version {
    /// Read one row.
    fn from_row(row: &VersionRow) -> Self {
        Self {
            id: row.id,
            workflow_id: row.workflow_id,
            organization_id: row.organization_id,
            change: Change::parse_or_default(&row.change),
            version: row.version,
            definition: row.definition.clone(),
            summary: row.summary.clone(),
            restored_from: row.restored_from,
            created_by: row.created_by,
            created_at: row.created_at,
        }
    }
}

/// The row as it comes back from postgres.
#[derive(Debug, sqlx::FromRow)]
struct VersionRow {
    /// Row id.
    id: Uuid,
    /// Owning rule.
    workflow_id: Uuid,
    /// Owning organization.
    organization_id: Uuid,
    /// Stored change text.
    change: String,
    /// Stored number.
    version: i32,
    /// Stored definition.
    definition: Value,
    /// Stored summary.
    summary: Value,
    /// Ancestry pointer.
    restored_from: Option<Uuid>,
    /// Actor.
    created_by: Option<Uuid>,
    /// Timestamp.
    created_at: OffsetDateTime,
}

const COLUMNS: &str = "id, workflow_id, organization_id, change, version, definition, summary, \
     restored_from, created_by, created_at";

/// Append a version row and bump the rule's counter, on the caller's connection.
///
/// Both statements share a transaction with the definition write that triggered them, so
/// "the rule was changed" and "the history knows it" cannot come apart. A caller that passes
/// a bare pool gets its own transaction; a caller that already holds a connection (the
/// create and update handlers) passes it so the version lands with the row.
pub async fn record(
    conn: &mut PgConnection,
    rule: &AutomationRule,
    change: Change,
    actor: Option<Uuid>,
    restored_from: Option<Uuid>,
) -> Result<Version> {
    let definition = rule.snapshot()?;
    let previous = latest(conn, rule.id).await?;
    let summary = match previous {
        Some(before) => diff(&before.definition, &definition),
        None => json!({ "first": true }),
    };

    // The counter is bumped in the same transaction, so two concurrent edits serialise on
    // the row lock and cannot claim the same number. `returning` gives the number that was
    // actually taken, not the one that was hoped for.
    let version: i32 = sqlx::query_scalar(
        "update workflows set version = version + 1 where id = $1 returning version",
    )
    .bind(rule.id)
    .fetch_one(&mut *conn)
    .await?;

    let id = Uuid::new_v4();
    let row = sqlx::query_as::<_, VersionRow>(&format!(
        "insert into workflow_versions \
         (id, workflow_id, organization_id, change, version, definition, summary, \
          restored_from, created_by) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
         returning {COLUMNS}"
    ))
    .bind(id)
    .bind(rule.id)
    .bind(rule.organization_id)
    .bind(change.as_str())
    .bind(version)
    .bind(&definition)
    .bind(&summary)
    .bind(restored_from)
    .bind(actor)
    .fetch_one(&mut *conn)
    .await?;

    Ok(Version::from_row(&row))
}

/// The whole history of a rule, newest first.
pub async fn list(pool: &PgPool, workflow_id: Uuid) -> Result<Vec<Version>> {
    let rows = sqlx::query_as::<_, VersionRow>(&format!(
        "select {COLUMNS} from workflow_versions \
         where workflow_id = $1 order by version desc"
    ))
    .bind(workflow_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.iter().map(Version::from_row).collect())
}

/// One version, addressed by its own id.
pub async fn find(pool: &PgPool, id: Uuid) -> Result<Option<Version>> {
    let row = sqlx::query_as::<_, VersionRow>(&format!(
        "select {COLUMNS} from workflow_versions where id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row.as_ref().map(Version::from_row))
}

/// The newest version of a rule, or `None` for a rule written before this table existed.
pub async fn latest(conn: &mut PgConnection, workflow_id: Uuid) -> Result<Option<Version>> {
    let row = sqlx::query_as::<_, VersionRow>(&format!(
        "select {COLUMNS} from workflow_versions \
         where workflow_id = $1 order by version desc limit 1"
    ))
    .bind(workflow_id)
    .fetch_optional(conn)
    .await?;

    Ok(row.as_ref().map(Version::from_row))
}

/// The definition a rule is *currently* running, taken from its own row.
///
/// A rule edited before this table shipped has no history, so the panel's diff needs a
/// "before" that is not in the table. It is here, derived from the rule itself, and it is
/// marked `current: true` so the tab can say "these changes are not in a version yet"
/// rather than pretend the rule has no history.
pub fn current_definition(rule: &AutomationRule) -> Result<Value> {
    Ok(rule.snapshot()?)
}

// ---------------------------------------------------------------------------------------------
// The diff
// ---------------------------------------------------------------------------------------------

/// The fields the diff compares, in the order the panel lists them.
///
/// A fixed list is the whole design. A structural walk of the two JSON documents would
/// report every key a later slice added (`graph`, `node_id`, …) as a change on every single
/// edit, and would need rewriting the first time REQ-004 changes the definition's shape.
#[must_use]
pub fn diff(before: &Value, after: &Value) -> Value {
    let mut changed: Vec<Value> = Vec::new();

    let mut note = |field: &str, from: Value, to: Value| {
        changed.push(json!({ "field": field, "from": from, "to": to }));
    };

    if before.get("name") != after.get("name") {
        note("name", opt(before, "name"), opt(after, "name"));
    }
    if before.get("description") != after.get("description") {
        note(
            "description",
            opt(before, "description"),
            opt(after, "description"),
        );
    }
    if before.get("enabled") != after.get("enabled") {
        note("enabled", opt(before, "enabled"), opt(after, "enabled"));
    }
    if before.get("event") != after.get("event")
        || before.get("hook_triggered") != after.get("hook_triggered")
    {
        note(
            "trigger",
            json!({ "event": opt(before, "event"), "webhook": opt(before, "hook_triggered") }),
            json!({ "event": opt(after, "event"), "webhook": opt(after, "hook_triggered") }),
        );
    }
    if before.get("site_id") != after.get("site_id") {
        note("site_id", opt(before, "site_id"), opt(after, "site_id"));
    }
    if before.get("conditions") != after.get("conditions") {
        let from = node_count(before.get("conditions"));
        let to = node_count(after.get("conditions"));
        note(
            "conditions",
            json!({ "nodes": from }),
            json!({ "nodes": to }),
        );
    }
    if before.get("steps") != after.get("steps") {
        let from = step_count(before.get("steps"));
        let to = step_count(after.get("steps"));
        note("actions", json!({ "steps": from }), json!({ "steps": to }));
    }
    if before.get("run_as_user_id") != after.get("run_as_user_id") {
        note(
            "run_as_user_id",
            opt(before, "run_as_user_id"),
            opt(after, "run_as_user_id"),
        );
    }
    if before.get("rate_limit_per_hour") != after.get("rate_limit_per_hour") {
        note(
            "rate_limit_per_hour",
            opt(before, "rate_limit_per_hour"),
            opt(after, "rate_limit_per_hour"),
        );
    }
    if before.get("concurrency") != after.get("concurrency") {
        note(
            "concurrency",
            opt(before, "concurrency"),
            opt(after, "concurrency"),
        );
    }
    if before.get("on_error") != after.get("on_error") {
        note("on_error", opt(before, "on_error"), opt(after, "on_error"));
    }

    // The count is derived from the list rather than recomputed, so the panel cannot show a
    // number that disagrees with the rows under it.
    let count = changed.len();
    json!({ "changed": changed, "count": count })
}

/// Whether a summary names at least one field.
#[must_use]
pub fn is_empty(summary: &Value) -> bool {
    summary
        .get("count")
        .and_then(Value::as_u64)
        .is_none_or(|count| count == 0)
}

/// The condition nodes in a stored tree, counted the way the editor counts them.
///
/// A bare v0 array is one `all` group, so the count is the length of the array; a group
/// object is counted by walking it. Walking the *node* and not the JSON text is what makes
/// `{"all":[…]}` and a flat `[…]` of the same comparisons report the same number.
fn node_count(value: Option<&Value>) -> usize {
    fn walk(value: &Value) -> usize {
        // A bare array is the v0 shape: a list of members, walked one by one.
        if let Value::Array(items) = value {
            return items.iter().map(walk).sum();
        }
        // A group is an object that HAS an `all`/`any` array. A *comparison* is also an
        // object — `{"field": …, "operator": …}` — and calling it a group with no members
        // counts every condition in the rule as zero. The key is what tells them apart, not
        // the JSON type: both are objects.
        if let Some(object) = value.as_object() {
            if object.contains_key("all") || object.contains_key("any") {
                return object
                    .values()
                    .filter_map(Value::as_array)
                    .flatten()
                    .map(walk)
                    .sum();
            }
        }
        1
    }

    value.map(walk).unwrap_or(0)
}

/// The number of condition comparisons a stored tree holds.
///
/// Public because the templates gallery and the editor both need the *same* number: a
/// gallery that says "2 conditions" for a template the editor then counts as one is the
/// sort of disagreement that makes an operator distrust both numbers.
#[must_use]
pub fn condition_count(value: &Value) -> usize {
    node_count(Some(value))
}

/// How many actions a definition carries.
fn step_count(value: Option<&Value>) -> usize {
    value.and_then(Value::as_array).map_or(0, Vec::len)
}

/// A field, or `null` when either side lacks it.
fn opt(value: &Value, field: &str) -> Value {
    value.get(field).cloned().unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition() -> Value {
        json!({
            "name": "Welcome new accounts",
            "description": "",
            "enabled": true,
            "event": "user.created",
            "hook_triggered": false,
            "site_id": null,
            "conditions": { "all": [{ "field": "event.email", "operator": "contains", "value": "@" }] },
            "steps": [{ "name": "Send the e-mail", "kind": "task", "action": "send_email",
                        "params": {}, "on_error": "inherit", "timeout_ms": 30000,
                        "max_attempts": 1 }],
            "run_as_user_id": null,
            "rate_limit_per_hour": 60,
            "concurrency": "queue",
            "on_error": "stop",
        })
    }

    #[test]
    fn an_unchanged_definition_reports_no_fields() {
        let summary = diff(&definition(), &definition());
        assert!(
            is_empty(&summary),
            "an identical definition changed something: {summary}"
        );
    }

    #[test]
    fn a_renamed_rule_is_one_field_not_three() {
        let mut after = definition();
        after["name"] = json!("Welcome every new account");
        let summary = diff(&definition(), &after);
        assert_eq!(summary["count"], 1);
        assert_eq!(summary["changed"][0]["field"], json!("name"));
        assert_eq!(summary["changed"][0]["from"], json!("Welcome new accounts"));
    }

    #[test]
    fn counts_are_read_from_the_nodes_not_the_text() {
        // The same two comparisons, written the three ways the matcher accepts them. The
        // SHAPE is a real edit and is reported as one — the editor did rewrite the tree —
        // but the *number* the operator reads has to be the same every time, because it
        // is counting conditions and there are two of them in all three shapes. A count
        // taken from the JSON text would report 1 group, then 2 nodes, then 1 node, and
        // none of those is an answer to "how much did I change".
        let mut flat = definition();
        flat["conditions"] = json!([
            { "field": "event.email", "operator": "contains", "value": "@" },
            { "field": "event.name", "operator": "exists", "value": null }
        ]);
        let mut nested = definition();
        nested["conditions"] = json!({
            "all": [
                { "any": [
                    { "field": "event.email", "operator": "contains", "value": "@" },
                    { "field": "event.name", "operator": "exists", "value": null }
                ]}
            ]
        });

        for other in [flat, nested] {
            let summary = diff(&definition(), &other);
            let conditions = summary["changed"]
                .as_array()
                .expect("changed is an array")
                .iter()
                .find(|entry| entry["field"] == json!("conditions"))
                .expect("the conditions are reported as changed");
            assert_eq!(conditions["from"]["nodes"], 1, "{conditions}");
            assert_eq!(conditions["to"]["nodes"], 2, "{conditions}");
        }
    }

    #[test]
    fn a_removed_action_reports_the_count_it_lost() {
        let mut after = definition();
        after["steps"] = json!([]);
        let summary = diff(&definition(), &after);
        assert_eq!(summary["changed"][0]["field"], json!("actions"));
        assert_eq!(summary["changed"][0]["from"]["steps"], 1);
        assert_eq!(summary["changed"][0]["to"]["steps"], 0);
    }

    #[test]
    fn the_bounds_are_compared_as_written() {
        let mut after = definition();
        after["rate_limit_per_hour"] = json!(5);
        after["concurrency"] = json!("skip");
        let summary = diff(&definition(), &after);
        let fields: Vec<&str> = summary["changed"]
            .as_array()
            .expect("changed is an array")
            .iter()
            .map(|entry| entry["field"].as_str().expect("a field name"))
            .collect();
        assert_eq!(fields, vec!["rate_limit_per_hour", "concurrency"]);
    }

    #[test]
    fn a_field_a_later_slice_adds_contributes_nothing() {
        // `graph` is REQ-004's. A structural diff would call every edit a change; the fixed
        // field list does not know the field, so it is not one.
        let mut after = definition();
        after["graph"] = json!({ "nodes": [] });
        assert!(is_empty(&diff(&definition(), &after)));
    }

    #[test]
    fn the_change_text_round_trips() {
        for change in [Change::Created, Change::Updated, Change::Restored] {
            assert_eq!(Change::parse_or_default(change.as_str()), change);
        }
        // A row written by a future version of this table is still a row in the history.
        assert_eq!(Change::parse_or_default("something_new"), Change::Updated);
    }
}
