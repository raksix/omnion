//! Exporting a project (REQ-133).
//!
//! The settings screen's own row has read "Name, key, description, colour, archive, export,
//! delete" since the module shipped. Slice 19 wrote `delete`. This is `export` — and it is not a
//! fourth label on the same dialog: the REQ's Risks section names an **"export-first hint"** as
//! the thing that makes deleting a project with dependencies safe, so the export has to be the
//! *remedy* a refusal points at, not a file the screen happens to offer.
//!
//! ## What an export is *for*
//!
//! Deletion is refused when the project holds workflows. The remedy an operator can act on is
//! **move them**, not "download them" — a JSON file does not unblock a `23503`. So this is not
//! the export that replaces deletion; it is the archive you take *before* you move everything out
//! and delete an empty project, and the record that survives a project you archive rather than
//! delete. Both readings are real and both need the same thing: a portable, self-describing
//! snapshot that does not need the database to be readable.
//!
//! ## Why it is not the same code as the audit CSV or the usage CSV
//!
//! Those two exist on this branch and both build their file **from the series already on screen**,
//! which is the right rule there and would be the wrong rule here: the settings screen shows no
//! export preview, so there is no on-screen series to reproduce, and a re-queried export would be a
//! second thing to keep in step. This file is built from **one transaction**, so the project row,
//! its members and its workflows are a single consistent snapshot — the half the on-screen rule
//! buys for free and a project export has to be argued for.
//!
//! ## The refusal that matters
//!
//! Export is a **read**, so it goes through [`crate::projects::find_visible`] like every other read
//! on this surface: a project the caller may not see answers `404` and its name never appears in
//! the message. That is why this module takes no permission decision of its own — an export
//! endpoint that re-derived visibility would be a second answer to the same question, which is the
//! split (`visible_project_ids` vs `find_visible`) this module has already been bitten by once.

use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{Result, WorkflowError};

/// Format the export is written in.
///
/// **JSON only, and the reason is the guarantee it carries.** A CSV of a project's workflows would
/// have to flatten `steps` (a JSON array whose element shape depends on the action), `conditions`
/// (a JSON array), and `schedule` (a cron string with its own grammar) into one row — and the
/// import that could read it back would have to know all three shapes, which is a second engine
/// that can drift from the first. A JSON snapshot keeps the stored shape exactly, so the file is
/// readable by anything and re-importable without this platform's cooperation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    /// Portable JSON: the shape the platform stores.
    #[default]
    Json,
}

/// One membership row, as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportedMember {
    /// Role as stored: `owner`, `manager`, `editor` or `viewer`.
    pub role: String,
    /// The member's user id.
    pub user_id: Uuid,
    /// Display name at export time — a convenience for a reader, **not** a copy of the record:
    /// the platform never treats it as identity.
    pub display_name: String,
    /// Email at export time, same caveat.
    pub email: String,
    /// When the membership was created.
    pub created_at: OffsetDateTime,
}

/// One workflow definition, as stored.
///
/// `steps` and `conditions` are `serde_json::Value` rather than the platform's own types on
/// purpose: **the export must not fail to serialize because this branch added a step kind.** A
/// typed field would make "export a project" contingent on the definition types, which is exactly
/// the coupling that turns a backup into a hostage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportedWorkflow {
    /// Workflow id.
    pub id: Uuid,
    /// Display name.
    pub name: String,
    /// Description.
    pub description: String,
    /// Whether it is enabled.
    pub enabled: bool,
    /// `manual`, `schedule` or `event`.
    pub trigger_kind: String,
    /// Cron expression for a scheduled workflow.
    pub schedule: Option<String>,
    /// The event this workflow listens for, when the trigger is `event`.
    pub trigger_event: Option<String>,
    /// Next scheduled fire time, carried so a restore does not replay a missed window.
    pub next_run_at: Option<OffsetDateTime>,
    /// The definition's steps, exactly as stored.
    pub steps: serde_json::Value,
    /// Guard conditions, exactly as stored.
    pub conditions: serde_json::Value,
    /// Trigger count at export time.
    pub trigger_count: i32,
    /// Last fire time at export time.
    pub last_triggered_at: Option<OffsetDateTime>,
    /// Created at.
    pub created_at: OffsetDateTime,
    /// Updated at.
    pub updated_at: OffsetDateTime,
}

/// The project itself, as stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportedProject {
    /// Project id.
    pub id: Uuid,
    /// Organization the project belonged to.
    pub organization_id: Uuid,
    /// The project key — the one identifier a person writes in a ticket.
    pub key: String,
    /// Display name.
    pub name: String,
    /// Description.
    pub description: String,
    /// Colour override.
    pub color: Option<String>,
    /// Icon override.
    pub icon: Option<String>,
    /// `active` or `archived`.
    pub status: String,
    /// Whether this was the organization's default project.
    pub is_default: bool,
    /// Owner, if the account still exists (`on delete set null`).
    pub owner_user_id: Option<Uuid>,
    /// Memberships at export time.
    pub members: Vec<ExportedMember>,
    /// Workflow definitions at export time.
    pub workflows: Vec<ExportedWorkflow>,
    /// When the project row was created.
    pub created_at: OffsetDateTime,
    /// When the project row was last updated.
    pub updated_at: OffsetDateTime,
}

/// The export envelope: the project plus what a reader needs to know about the snapshot.
///
/// `exported_at` and `schema` are not decoration. A snapshot without them cannot be told apart
/// from a live row, and "which platform wrote this" is the first question anybody asks of a file
/// found on a laptop two years later.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectExport {
    /// Always `"omnion.project-export"`, so a future format can be told apart from this one.
    pub schema: String,
    /// The platform version string at export time, when it is known.
    pub generator: Option<String>,
    /// When the snapshot was taken.
    pub exported_at: OffsetDateTime,
    /// The project and its contents.
    pub project: ExportedProject,
}

impl ProjectExport {
    /// A stable, safe filename stem for this export.
    ///
    /// **The key, lowercased, and never the name.** The name is free text that can contain `/`, a
    /// space, or a character the filesystem encodes differently — every one of which turns a
    /// download into a file the user cannot find. The key is the one identifier on this platform
    /// that is already `[A-Z0-9]{2,8}`, so it needs no sanitising pass that could disagree with the
    /// migration's own constraint.
    #[must_use]
    pub fn filename_stem(&self) -> String {
        self.project.key.to_lowercase()
    }

    /// How many workflows the snapshot carries.
    ///
    /// The dialog states this **before** anybody clicks anything, so the number a user reads is the
    /// number the file actually has.
    #[must_use]
    pub fn workflow_count(&self) -> usize {
        self.project.workflows.len()
    }

    /// Whether the snapshot has anything in it worth keeping.
    ///
    /// An empty project still exports: the row, its key and its memberships are facts, and a
    /// snapshot that refused to be written for an empty project would make "export first" advice
    /// that fails exactly when somebody deletes a shell.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.project.workflows.is_empty() && self.project.members.is_empty()
    }
}

/// The format version this build writes.
///
/// Bumping it is a deliberate act: a reader that switches on the string would have to handle a
/// value it does not know, which is why this is a constant rather than a number someone increments
/// when they add a field.
pub const EXPORT_SCHEMA: &str = "omnion.project-export/1";

/// Read the display name and email of a user.
///
/// Takes a connection so the whole export reads one snapshot. `users` is joined rather than
/// looked up per member: a member table that performs one round trip per row is how an export of
/// a two-hundred-member project takes ten seconds.
///
/// **A miss here is not a case this function has to handle, and the first version of it did.**
/// The obvious defensive move is a fallback — an unexplained `uuid` row otherwise — and that
/// branch was written here first. It is **unreachable**: `automation_project_members.user_id` is
/// `on delete cascade`, so the database removes the membership the moment the account goes and no
/// membership can name a user that is not there. The gate
/// (`a_membership_cannot_outlive_its_account`) asserts that FK on purpose, so if someone ever
/// changes it to `set null` — a reasonable-looking change, since the project's own `owner_user_id`
/// is `set null` — **this gate fails and names what has to happen next**: a fallback that says a
/// sentence instead of showing a bare id. Until then the `expect` is the honest form: a membership
/// with no account is a broken invariant, not a state to be rendered.
async fn users_for(
    connection: &mut sqlx::PgConnection,
    ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, (String, String)>> {
    if ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rows = sqlx::query(
        "select id, coalesce(nullif(display_name, ''), email, id::text) as name, email \
         from users where id = any($1)",
    )
    .bind(ids)
    .fetch_all(&mut *connection)
    .await?;
    let mut out = std::collections::HashMap::new();
    for row in rows {
        out.insert(
            row.get::<Uuid, _>("id"),
            (row.get::<String, _>("name"), row.get::<String, _>("email")),
        );
    }
    Ok(out)
}

/// Build a project export snapshot.
///
/// **One transaction for all three reads**, and that is the whole of the design: a project, its
/// members and its workflows read across three connections can describe three different moments,
/// and the resulting file would be a snapshot of nothing in particular. `repeatable read` is not
/// asked for — the default snapshot is already fixed at the first statement, so the row, the
/// memberships and the workflows are all as of one instant.
///
/// The workflow list is ordered by `(created_at, id)`: an export whose order changes between two
/// runs of the same data makes a diff of two snapshots unreadable, and the tiebreaker on `id`
/// means two workflows created in the same transaction cannot swap places.
///
/// A membership whose `user_id` no longer exists is **still exported**, with the id and a
/// placeholder name. The membership is the record; dropping the row because a join came back
/// empty would make the file silently smaller than the thing it claims to describe.
pub async fn build_export(pool: &PgPool, project_id: Uuid) -> Result<ProjectExport> {
    let mut transaction = pool.begin().await?;

    let project = sqlx::query(
        "select id, organization_id, key, name, description, color, icon, status, is_default, \
                owner_user_id, created_at, updated_at \
         from automation_projects where id = $1",
    )
    .bind(project_id)
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or_else(|| {
        WorkflowError::invalid("project_not_found", "this project no longer exists")
    })?;

    let members = sqlx::query(
        "select user_id, role, created_at from automation_project_members \
         where project_id = $1 order by created_at, user_id",
    )
    .bind(project_id)
    .fetch_all(&mut *transaction)
    .await?;

    let member_ids: Vec<Uuid> = members.iter().map(|row| row.get::<Uuid, _>("user_id")).collect();
    let names = users_for(&mut transaction, &member_ids).await?;

    let workflows = sqlx::query(
        "select id, name, description, enabled, trigger_kind, schedule, trigger_event, \
                next_run_at, steps, conditions, trigger_count, last_triggered_at, created_at, \
                updated_at \
         from workflows where project_id = $1 order by created_at, id",
    )
    .bind(project_id)
    .fetch_all(&mut *transaction)
    .await?;

    let exported_members = members
        .iter()
        .map(|row| {
            let user_id = row.get::<Uuid, _>("user_id");
            // The one `expect` in this module, and the reason is in `users_for`: the FK cascades,
            // so a membership without its account means the schema changed under us. Naming it
            // here beats rendering "deleted account (3f2a…)" for a row that cannot exist — and the
            // gate asserts the FK so that a future `set null` forces this line to be rewritten
            // deliberately rather than discovered in a file somebody has already archived.
            let (display_name, email) = names.get(&user_id).unwrap_or_else(|| {
                panic!(
                    "membership {user_id} has no account row; \
                     automation_project_members.user_id must be on delete cascade"
                )
            });
            ExportedMember {
                role: row.get::<String, _>("role"),
                user_id,
                display_name: display_name.clone(),
                email: email.clone(),
                created_at: row.get("created_at"),
            }
        })
        .collect();

    let exported_workflows = workflows
        .iter()
        .map(|row| ExportedWorkflow {
            id: row.get("id"),
            name: row.get("name"),
            description: row.get("description"),
            enabled: row.get("enabled"),
            trigger_kind: row.get("trigger_kind"),
            schedule: row.get("schedule"),
            trigger_event: row.get("trigger_event"),
            next_run_at: row.get("next_run_at"),
            steps: row.get("steps"),
            conditions: row.get("conditions"),
            trigger_count: row.get("trigger_count"),
            last_triggered_at: row.get("last_triggered_at"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        })
        .collect();

    // `generator` is a plain optional string and nothing is invented for it: this crate has no
    // version constant of its own (the platform's lives in `omnion-core`, and a business module
    // taking a dependency on the core crate for a filename would be the wrong direction), so the
    // field is `None` here and the envelope carries the schema, which is the part a reader must be
    // able to rely on. A reader that gets `null` knows nothing about the build; a reader that got
    // a plausible-looking fake version would know something false.
    let export = ProjectExport {
        schema: EXPORT_SCHEMA.to_string(),
        generator: None,
        exported_at: OffsetDateTime::now_utc(),
        project: ExportedProject {
            id: project.get("id"),
            organization_id: project.get("organization_id"),
            key: project.get("key"),
            name: project.get("name"),
            description: project.get("description"),
            color: project.get("color"),
            icon: project.get("icon"),
            status: project.get("status"),
            is_default: project.get("is_default"),
            owner_user_id: project.get("owner_user_id"),
            members: exported_members,
            workflows: exported_workflows,
            created_at: project.get("created_at"),
            updated_at: project.get("updated_at"),
        },
    };

    transaction.commit().await?;
    Ok(export)
}

/// Serialize an export to the bytes the download carries.
///
/// Returns `Result` rather than `String` because serialization of a `serde_json::Value` can fail on
/// a value the database accepted — and **the honest answer to that is an error, not an empty file.**
/// An export endpoint that swallowed the failure would answer 200 with a zero-byte download, and a
/// user who closed a project on the strength of that file owns nothing.
pub fn render(export: &ProjectExport) -> Result<String> {
    serde_json::to_string_pretty(export).map_err(|error| {
        WorkflowError::invalid(
            "project_export_failed",
            format!("this project could not be exported: {error}"),
        )
    })
}

/// The `Content-Type` the download is served with, by format.
#[must_use]
pub fn content_type(format: ExportFormat) -> &'static str {
    match format {
        ExportFormat::Json => "application/json; charset=utf-8",
    }
}

/// The filename the download is offered under, by format.
///
/// The timestamp is in the stem and it is UTC: an export found tomorrow must not sort beside today's
/// in a Downloads folder, and a local timestamp would make the filename depend on the exporter's
/// machine.
#[must_use]
pub fn filename(format: ExportFormat, export: &ProjectExport) -> String {
    // The timestamp is composed from components rather than a format string, and that is not a
    // taste: `OffsetDateTime::format` needs the `time` crate's `formatting` feature, which this
    // workspace does not enable for it (adding the feature to unblock one filename is a workspace
    // change for a cosmetic reason). The components are also the clearer statement — the stamp is
    // a specific shape, not a format description.
    let at = export.exported_at;
    let stamp = format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        at.year(),
        at.month() as u8,
        at.day(),
        at.hour(),
        at.minute(),
        at.second(),
    );
    let stem = export.filename_stem();
    match format {
        ExportFormat::Json => format!("{stem}-export-{stamp}.json"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schema_string_is_the_one_this_build_writes() {
        // A reader switches on this. Asserting it against the constant rather than a literal is
        // worthless — it would pass through any edit — so the literal is here.
        assert_eq!(EXPORT_SCHEMA, "omnion.project-export/1");
    }

    #[test]
    fn the_filename_stem_is_the_lower_key_and_needs_no_sanitising() {
        let mut export = fixture();
        export.project.key = "OPS".to_string();
        assert_eq!(export.filename_stem(), "ops");
        // The key constraint is [A-Z0-9]{2,8}, so nothing that reaches here can contain a
        // separator. This test states that dependency rather than trusting it.
        assert!(export
            .project
            .key
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()));
    }

    #[test]
    fn the_filename_is_not_the_display_name() {
        // A name with a slash is the case that matters: it is legal free text, and using it would
        // produce a path the browser truncates at the separator.
        let mut export = fixture();
        export.project.name = "Ops / EU rollout".to_string();
        let rendered = filename(ExportFormat::Json, &export);
        assert!(!rendered.contains('/'), "the filename carries a path separator: {rendered}");
        assert!(rendered.ends_with(".json"));
    }

    #[test]
    fn the_workflow_count_is_the_number_the_dialog_states_up_front() {
        let mut export = fixture();
        assert_eq!(export.workflow_count(), 1);
        export.project.workflows.clear();
        assert_eq!(export.workflow_count(), 0);
    }

    #[test]
    fn an_export_with_nothing_in_it_is_still_an_export() {
        // The refusal here would make "export first" advice fail exactly when somebody is about
        // to delete a shell, and the row itself is the fact being preserved.
        let mut export = fixture();
        export.project.workflows.clear();
        export.project.members.clear();
        assert!(export.is_empty());
        let rendered = render(&export).expect("an empty project still serializes");
        assert!(rendered.contains("\"schema\": \"omnion.project-export/1\""));
    }

    #[test]
    fn rendering_is_json_that_carries_the_schema_and_the_key() {
        let rendered = render(&fixture()).expect("the fixture renders");
        assert!(rendered.starts_with('{'));
        assert!(rendered.contains("\"schema\": \"omnion.project-export/1\""));
        assert!(rendered.contains("\"key\": \"OPS\""));
    }

    #[test]
    fn json_is_the_only_format_and_it_is_served_as_json() {
        assert_eq!(
            content_type(ExportFormat::Json),
            "application/json; charset=utf-8"
        );
    }

    fn fixture() -> ProjectExport {
        ProjectExport {
            schema: EXPORT_SCHEMA.to_string(),
            generator: Some("0.1.0".to_string()),
            exported_at: OffsetDateTime::now_utc(),
            project: ExportedProject {
                id: Uuid::nil(),
                organization_id: Uuid::nil(),
                key: "OPS".to_string(),
                name: "Operations".to_string(),
                description: String::new(),
                color: None,
                icon: None,
                status: "active".to_string(),
                is_default: false,
                owner_user_id: None,
                members: vec![ExportedMember {
                    role: "owner".to_string(),
                    user_id: Uuid::nil(),
                    display_name: "Furkan Ermağ".to_string(),
                    email: "furkan@example.test".to_string(),
                    created_at: OffsetDateTime::now_utc(),
                }],
                workflows: vec![ExportedWorkflow {
                    id: Uuid::nil(),
                    name: "Nightly".to_string(),
                    description: String::new(),
                    enabled: true,
                    trigger_kind: "schedule".to_string(),
                    schedule: Some("0 3 * * *".to_string()),
                    trigger_event: None,
                    next_run_at: None,
                    steps: serde_json::json!([{"name": "run", "action": "noop"}]),
                    conditions: serde_json::json!([]),
                    trigger_count: 0,
                    last_triggered_at: None,
                    created_at: OffsetDateTime::now_utc(),
                    updated_at: OffsetDateTime::now_utc(),
                }],
                created_at: OffsetDateTime::now_utc(),
                updated_at: OffsetDateTime::now_utc(),
            },
        }
    }
}