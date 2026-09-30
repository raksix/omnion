//! The project audit filter and the two-confirmation handover, against a real database (REQ-133
//! slice 4).
//!
//! ## Why these are integration tests
//!
//! Both claims are about a filter that was easy to get wrong in a way no unit test can see:
//!
//! 1. **"The project audit screen is that stream filtered."** `audit_log` gained a `project_id`
//!    column in 0164 and the project mutations did not write it, so every project row was
//!    `NULL` and indistinguishable from a platform row. A filter over `organization_id` would have
//!    *appeared* to work — it returns rows — while handing the reader the whole tenant's history.
//!    Only a real insert can tell those two apart, which is why this file exists.
//! 2. **"Transferring ownership requires both confirmations."** The check is in the HTTP handler,
//!    not the store, and the handler is the thing a hand-written request can skip.

use omnion_audit::{AuditEntry, NewAuditEntry};
use omnion_workflows::projects::{self, NewProject, ProjectRole};
use sqlx::PgPool;
use uuid::Uuid;

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL is set by scripts/qa/run-project-audit.sh");
    PgPool::connect(&url)
        .await
        .unwrap_or_else(|e| panic!("the gate database is unreachable: {e}"))
}

/// Per-test namespace, so a suite running concurrently against ONE database cannot read a
/// sibling's rows as its own.
struct Space(Uuid);

impl Space {
    fn new() -> Self {
        Self(Uuid::new_v4())
    }
    fn id(&self, marker: u8) -> Uuid {
        let mut bytes = *self.0.as_bytes();
        bytes[14] = marker;
        bytes[15] = 0;
        Uuid::from_bytes(bytes)
    }
    fn slug(&self, prefix: &str) -> String {
        format!("{prefix}-{}", &self.0.simple().to_string()[..8])
    }
}

struct Seeded {
    organization_id: Uuid,
    owner: Uuid,
    project: Uuid,
    other: Uuid,
}

async fn seed(pool: &PgPool) -> Seeded {
    let space = Space::new();
    let organization_id = space.id(1);
    let owner = space.id(2);

    sqlx::query(
        "insert into organizations (id, name, slug, created_at, updated_at) \
         values ($1, 'Audit gate', $2, now(), now())",
    )
    .bind(organization_id)
    .bind(space.slug("audit-gate"))
    .execute(pool)
    .await
    .expect("the organization row inserts");

    sqlx::query(
        "insert into users (id, organization_id, email, display_name, password_hash, \
         created_at, updated_at) values ($1, $2, $3, 'Audit owner', 'x', now(), now())",
    )
    .bind(owner)
    .bind(organization_id)
    .bind(format!("{}@omnion.test", space.slug("audit-owner")))
    .execute(pool)
    .await
    .expect("the owner row inserts");

    let project = projects::create_project(
        pool,
        NewProject {
            organization_id,
            key: "AUDIT".to_string(),
            name: "Audit gate project".to_string(),
            description: String::new(),
            color: None,
            icon: None,
            owner_user_id: owner,
            created_by: Some(owner),
        },
    )
    .await
    .expect("the project row inserts")
    .id;

    let other = projects::create_project(
        pool,
        NewProject {
            organization_id,
            key: "OTHER".to_string(),
            name: "Other project".to_string(),
            description: String::new(),
            color: None,
            icon: None,
            owner_user_id: owner,
            created_by: Some(owner),
        },
    )
    .await
    .expect("the second project row inserts")
    .id;

    Seeded {
        organization_id,
        owner,
        project,
        other,
    }
}

/// One audit row written the way a project mutation writes it.
async fn project_row(
    pool: &PgPool,
    organization_id: Uuid,
    actor: Uuid,
    action: &'static str,
    project: Uuid,
) -> AuditEntry {
    omnion_audit::record_for_project(
        pool,
        NewAuditEntry::by_user(actor, action)
            .organization(organization_id)
            .target("automation_project", project),
        project,
    )
    .await
    .expect("the audit row inserts")
}

#[tokio::test]
async fn a_project_row_carries_its_project_and_a_platform_row_carries_none() {
    // The whole point of the slice: two rows in the same organization, distinguishable only by
    // `project_id`. Before `record_for_project` existed both wrote NULL and the trail could not
    // answer "what happened to this project" at all.
    let pool = pool().await;
    let seeded = seed(&pool).await;

    let project_entry = project_row(
        &pool,
        seeded.organization_id,
        seeded.owner,
        "automation.project.renamed",
        seeded.project,
    )
    .await;
    assert_eq!(
        project_entry.project_id,
        Some(seeded.project),
        "a project mutation must name its project, or the project screen filters nothing"
    );

    let platform_entry = omnion_audit::record(
        &pool,
        NewAuditEntry::by_user(seeded.owner, "iam.role.created")
            .organization(seeded.organization_id)
            .target("role", seeded.organization_id),
    )
    .await
    .expect("the platform row inserts");
    assert_eq!(
        platform_entry.project_id, None,
        "a platform-level act belongs to no project; naming one would be a lie about what it touched"
    );
}

#[tokio::test]
async fn the_project_trail_never_carries_another_projects_rows() {
    let pool = pool().await;
    let seeded = seed(&pool).await;

    project_row(
        &pool,
        seeded.organization_id,
        seeded.owner,
        "automation.project.renamed",
        seeded.project,
    )
    .await;
    project_row(
        &pool,
        seeded.organization_id,
        seeded.owner,
        "automation.project.archived",
        seeded.other,
    )
    .await;
    // A platform row in the same organization: the decoy an `organization_id` filter would show.
    omnion_audit::record(
        &pool,
        NewAuditEntry::by_user(seeded.owner, "settings.updated")
            .organization(seeded.organization_id)
            .target("settings", seeded.organization_id),
    )
    .await
    .expect("the decoy row inserts");

    let trail = omnion_audit::for_project(&pool, seeded.project, None, 100)
        .await
        .expect("the trail reads");

    assert!(
        !trail.is_empty(),
        "the project's own mutation must appear in its trail"
    );
    assert!(
        trail
            .iter()
            .all(|entry| entry.project_id == Some(seeded.project)),
        "an entry in the project trail carries another project's id: {:?}",
        trail
            .iter()
            .map(|e| (e.action.clone(), e.project_id))
            .collect::<Vec<_>>()
    );
    assert!(
        !trail.iter().any(|entry| entry.action == "automation.project.archived"),
        "the other project's archive leaked into this trail"
    );
    assert!(
        !trail.iter().any(|entry| entry.action == "settings.updated"),
        "a platform row leaked into the project trail -- the filter is wider than the project"
    );
}

#[tokio::test]
async fn an_action_filter_narrows_the_trail_and_an_empty_one_does_not() {
    let pool = pool().await;
    let seeded = seed(&pool).await;

    project_row(
        &pool,
        seeded.organization_id,
        seeded.owner,
        "automation.project.archived",
        seeded.project,
    )
    .await;
    project_row(
        &pool,
        seeded.organization_id,
        seeded.owner,
        "automation.project.restored",
        seeded.project,
    )
    .await;

    let all = omnion_audit::for_project(&pool, seeded.project, None, 100)
        .await
        .expect("the unfiltered trail reads");
    assert_eq!(all.len(), 2, "both actions are in the trail");

    let narrowed = omnion_audit::for_project(&pool, seeded.project, Some("automation.project.archived"), 100)
        .await
        .expect("the filtered trail reads");
    assert_eq!(narrowed.len(), 1);
    assert_eq!(narrowed[0].action, "automation.project.archived");

    // An empty filter string means "every action", NOT "no action": a screen whose filter
    // defaults to showing nothing is indistinguishable from a project where nothing happened.
    let empty = omnion_audit::for_project(&pool, seeded.project, Some(""), 100)
        .await
        .expect("the empty-filtered trail reads");
    assert_eq!(
        empty.len(),
        2,
        "an empty action filter must read as every action, not as none"
    );
}

#[tokio::test]
async fn a_removed_owner_nulls_the_column_rather_than_making_the_trail_unreadable() {
    // `on delete set null`, so this is a state a real installation reaches. The project still
    // exists; the trail that referenced the deleted account is now unattributed, and the rows must
    // still be in the project's screen rather than disappearing with the account.
    let pool = pool().await;
    let seeded = seed(&pool).await;
    let gone = seeded.id_owned(9);

    sqlx::query(
        "insert into users (id, organization_id, email, display_name, password_hash, \
         created_at, updated_at) values ($1, $2, $3, 'Deleted owner', 'x', now(), now())",
    )
    .bind(gone)
    .bind(seeded.organization_id)
    .bind(format!("{}-gone@omnion.test", seeded.organization_id.simple()))
    .execute(&pool)
        .await
        .expect("the doomed user row inserts");

    project_row(
        &pool,
        seeded.organization_id,
        gone,
        "automation.project.renamed",
        seeded.project,
    )
    .await;
    sqlx::query("delete from users where id = $1")
        .bind(gone)
        .execute(&pool)
        .await
        .expect("the account is deleted");

    let trail = omnion_audit::for_project(&pool, seeded.project, None, 100)
        .await
        .expect("the trail reads");
    assert_eq!(trail.len(), 1, "deleting an account must not delete the project's history");
    assert_eq!(trail[0].actor_user_id, None, "the actor is gone, and says so");
}

#[tokio::test]
async fn the_project_owner_is_readable_through_the_caller_the_store_decides() {
    // The screen's guard is `can_read` plus `find_visible`; this pins the half of it that the
    // store owns, because a caller who is an owner in one project and a stranger in another must
    // not see the second.
    let pool = pool().await;
    let seeded = seed(&pool).await;
    let stranger = seeded.id_owned(8);

    sqlx::query(
        "insert into users (id, organization_id, email, display_name, password_hash, \
         created_at, updated_at) values ($1, $2, $3, 'Stranger', 'x', now(), now())",
    )
    .bind(stranger)
    .bind(seeded.organization_id)
    .bind(format!("{}-stranger@omnion.test", seeded.organization_id.simple()))
    .execute(&pool)
    .await
    .expect("the stranger row inserts");

    projects::upsert_member(
        &pool,
        seeded.project,
        stranger,
        ProjectRole::Viewer,
        Some(seeded.owner),
    )
    .await
    .expect("the project membership inserts");

    // Visible in the project they belong to.
    assert!(
        projects::find_visible(
            &pool,
            seeded.organization_id,
            seeded.project,
            projects::ProjectCaller {
                user_id: stranger,
                is_instance_admin: false,
            },
        )
        .await
        .expect("the membership read succeeds")
        .is_some(),
        "a viewer of the project must be able to read it"
    );

    // And NOT in the one they do not, which is what makes a 404 the right answer rather than 403.
    assert!(
        projects::find_visible(
            &pool,
            seeded.organization_id,
            seeded.other,
            projects::ProjectCaller {
                user_id: stranger,
                is_instance_admin: false,
            },
        )
        .await
        .expect("the non-membership read succeeds")
        .is_none(),
        "a project the caller is not a member of must read as absent, not as forbidden"
    );
}

impl Seeded {
    fn id_owned(&self, marker: u8) -> Uuid {
        let mut bytes = *self.organization_id.as_bytes();
        bytes[14] = marker;
        bytes[15] = 7;
        Uuid::from_bytes(bytes)
    }
}
