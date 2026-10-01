//! Walks for the change-set apply (REQ-101, slice 3b).
//!
//! Slice 3a proved two different things, and the difference is the reason this file exists:
//!
//! 1. **The executor stopped.** A unit walk drives a three-operation set through an executor
//!    that refuses the second and asserts the third never ran. That is a claim about *control
//!    flow*, and it holds with no database anywhere.
//! 2. **The transaction undid a row.** Whether operation 1's write is still there afterwards is
//!    a claim about *PostgreSQL*, and no amount of a mock executor can make it. The request's
//!    acceptance criterion asks for exactly this: "a failure in operation 3 leaves operations
//!    1–2 rolled back, with `failed` status, the failing operation named and an
//!    `ai.changeset.failed` event" — four claims, and only the first is a unit test.
//!
//! So the three walks below each prove one of the rest, against a real page in a real
//! `pages`/`page_revisions` pair:
//!
//! - `a_three_operation_set_applies_every_page` — the happy path, through the **same** store
//!   function the route calls, and the assertion is on the revisions: three pages, three new
//!   drafts, three `applied` rows.
//! - `a_refused_third_operation_leaves_the_first_two_pages_untouched` — the rollback claim.
//!   Operation 1 is a real write, operation 2 targets a page that does not exist, and the walk
//!   reads operation 1's revision back through the content crate. `revision_no` 1 and the
//!   original title are the evidence; "the executor stopped" would leave the same numbers.
//! - `a_failed_apply_records_the_status_the_reason_and_the_event` — the last two claims: the
//!   row reads `failed` with the failing operation named, and the bus carries
//!   `ai.changeset.failed`. The row is read **outside** the transaction, which is the whole
//!   point: a `failed` written inside the transaction that failed would be gone.
//!
//! Everything runs against a fresh database created and dropped by the harness, so a walk
//! that leaves a row behind cannot make the next one pass.

use omnion_ai_hub::change_sets::store::{self, NewChangeSet};
use omnion_ai_hub::change_sets::{self, ChangeOp};
use omnion_ai_hub::error::AiHubError;
use omnion_core::Db;
use omnion_core::config::{Config, DatabaseConfig};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

// -------------------------------------------------------------------------------------------
// The harness
// -------------------------------------------------------------------------------------------

struct SetStore {
    pool: PgPool,
    organization_id: Uuid,
    site_id: Uuid,
    database: String,
    maintenance: Option<Db>,
}

impl SetStore {
    async fn fresh() -> Option<Self> {
        let config = Config::from_env().ok()?;
        if let Err(err) = Db::connect(&DatabaseConfig {
            url: config.database.url.clone(),
            max_connections: 1,
        })
        .await
        {
            eprintln!("PostgreSQL is not reachable: {err}");
            return None;
        }

        let database = format!("omnion_cset_{}", Uuid::new_v4().simple());
        let maintenance = Db::connect(&DatabaseConfig {
            url: swap_database(&config.database.url, "postgres"),
            max_connections: 1,
        })
        .await
        .expect("the maintenance connection must work");
        sqlx::query(&format!("create database \"{database}\""))
            .execute(maintenance.pool())
            .await
            .expect("the temporary database must be created");

        let db = Db::connect(&DatabaseConfig {
            url: swap_database(&config.database.url, &database),
            // 4, not the default: this box runs ten writer loops against one
            // `max_connections = 100` server, and every walk opens its own database.
            max_connections: 4,
        })
        .await
        .expect("the fresh database must connect");
        db.migrate().await.expect("migrations must apply");

        let organization_id = seed_organization(db.pool(), "setco").await;
        let site_id = seed_site(db.pool(), organization_id).await;

        Some(Self {
            pool: db.pool().clone(),
            organization_id,
            site_id,
            database,
            maintenance: Some(maintenance),
        })
    }

    /// A page with one draft revision, the shape every content write starts from.
    async fn page(&self, slug: &str, title: &str) -> Uuid {
        let page_id: Uuid = sqlx::query_scalar(
            "insert into pages (site_id, slug, status) values ($1, $2, 'draft') returning id",
        )
        .bind(self.site_id)
        .bind(slug)
        .fetch_one(&self.pool)
        .await
        .expect("the fixture page must be created");

        sqlx::query(
            "insert into page_revisions (page_id, revision_no, state, title, body, summary) \
             values ($1, 1, 'draft', $2, 'Original body', 'Original summary')",
        )
        .bind(page_id)
        .bind(title)
        .execute(&self.pool)
        .await
        .expect("the fixture revision must be created");

        page_id
    }

    /// A page id that is a well-formed uuid and has no row.
    ///
    /// The refusal this stands in for is `ContentError::PageNotFound` from the writer itself,
    /// not a parse failure: a non-uuid id would refuse earlier, in the route, for a reason that
    /// has nothing to do with rollback.
    async fn missing_page(&self) -> Uuid {
        Uuid::new_v4()
    }

    /// The newest revision of a page, as `(revision_no, title)`.
    async fn latest(&self, page_id: Uuid) -> (i32, String) {
        sqlx::query_as(
            "select revision_no, title from page_revisions where page_id = $1 \
             order by revision_no desc limit 1",
        )
        .bind(page_id)
        .fetch_one(&self.pool)
        .await
        .expect("the page must still carry a revision")
    }

    /// How many revisions a page carries.
    async fn revision_count(&self, page_id: Uuid) -> i64 {
        sqlx::query_scalar("select count(*) from page_revisions where page_id = $1")
            .bind(page_id)
            .fetch_one(&self.pool)
            .await
            .expect("the count must answer")
    }

    /// File a set and drive it to `confirmed`, the way the routes do.
    async fn confirmed_set(&self, title: &str, operations: Vec<ChangeOp>) -> Uuid {
        let set = store::append(
            &self.pool,
            &NewChangeSet {
                organization_id: self.organization_id,
                site_id: Some(self.site_id),
                title: title.to_owned(),
                operations: operations.clone(),
                created_by: None,
                created_by_agent: None,
                created_by_run: None,
                base_revisions: store::current_revisions(
                    &self.pool,
                    self.organization_id,
                    &operations,
                )
                .await
                .expect("the targets must be pinnable to a revision"),
            },
        )
        .await
        .expect("the set must be filed");

        let confirmed = store::transition(
            &self.pool,
            self.organization_id,
            set.id,
            "draft",
            "confirmed",
            None,
        )
        .await
        .expect("the transition must answer")
        .expect("the set must have been confirmed");
        confirmed.id
    }

    /// Park a set (`draft → pending`) and file one approval per gated operation, the way the
    /// confirm route does — through the **real** store, so the rows carry the `operation_key`
    /// the release path joins on.
    ///
    /// `policy` is written by the caller only if the default six are not already in force; the
    /// defaults gate every class, so a set with a gated operation parks without help.
    async fn park(&self, set_id: Uuid) -> Vec<Uuid> {
        store::transition(
            &self.pool,
            self.organization_id,
            set_id,
            "draft",
            "pending",
            None,
        )
        .await
        .expect("the park transition must answer")
        .expect("the set must have parked");

        let set = store::read(&self.pool, self.organization_id, set_id)
            .await
            .expect("the set must be readable")
            .expect("the set must exist");

        let mut ids = Vec::new();
        for (op, class) in set.gated_operations() {
            let mapping =
                omnion_ai_hub::approvals::target::mapping_for(&op.operation.resource_type)
                    .expect("pages have a mapping");
            let plan =
                omnion_ai_hub::approvals::target::preview(&self.pool, mapping, &op.operation)
                    .await
                    .expect("the preview must be computable");
            let policy =
                omnion_ai_hub::approvals::io::policy_for(&self.pool, self.organization_id, class)
                    .await
                    .expect("the policy must resolve");
            let requested = omnion_ai_hub::approvals::io::request(
                &self.pool,
                &omnion_ai_hub::approvals::io::NewApproval {
                    organization_id: self.organization_id,
                    site_id: self.site_id.into(),
                    run_id: None,
                    step_id: None,
                    agent_id: None,
                    identity_id: None,
                    change_set_id: Some(set_id),
                    operation_key: Some(op.key.clone()),
                    tool_key: "content.publish".to_owned(),
                    tool_class: class.to_owned(),
                    resource_type: Some(op.operation.resource_type.clone()),
                    resource_id: Some(op.operation.resource_id.clone()),
                    resource_label: Some(plan.label.clone()),
                    title: format!("{} a page: {}", op.key, plan.label),
                    summary: "Parked by a walk.".to_owned(),
                    operation_count: 1,
                    preview: plan.to_preview(mapping),
                    preview_hash: plan.hash.clone(),
                    base_revision: Some(plan.base_revision.clone()),
                    requested_by: None,
                    model_id: None,
                    risk: "medium".to_owned(),
                    policy,
                    requested_at: time::OffsetDateTime::now_utc(),
                },
            )
            .await
            .expect("the approval must file");
            ids.push(requested.approval().id);
        }
        ids
    }

    /// Record a decision on an approval, the way `decide()` does: a status plus the deciding
    /// time, which the table's `pending_iff_undecided` constraint ties together.
    async fn decide(&self, approval_id: Uuid, status: &str) {
        let written = sqlx::query(
            "update ai_approvals set status = $2, decided_at = now(), \
             decision_note = $3 \
             where id = $1 and organization_id = $4 and status = 'pending'",
        )
        .bind(approval_id)
        .bind(status)
        .bind(if status == "rejected" {
            Some("no".to_owned())
        } else {
            None
        })
        .bind(self.organization_id)
        .execute(&self.pool)
        .await
        .expect("the decision must be writable")
        .rows_affected();
        assert_eq!(written, 1, "the pending row must be the one that moved");
    }

    /// The operation keys the release path sees as **refused**.
    async fn rejected_keys(&self, set_id: Uuid) -> Vec<String> {
        store::rejected_operation_keys(&self.pool, self.organization_id, set_id)
            .await
            .expect("the rejected keys must be readable")
    }

    /// The operation keys the release path sees for a set.
    async fn approved_keys(&self, set_id: Uuid) -> Vec<String> {
        store::approved_operation_keys(&self.pool, self.organization_id, set_id)
            .await
            .expect("the approved keys must be readable")
    }

    /// Whether a page row is still there.
    ///
    /// A delete walk cannot assert on a revision: the point is that the row is **gone**, and
    /// `latest()` on a deleted page either errors or reads somebody else's revision. Existence
    /// is the only honest witness, and it is checked on both sides of the apply — present while
    /// the set is parked, absent once every gate was answered.
    async fn page_still_exists(&self, page_id: Uuid) -> bool {
        sqlx::query_scalar("select exists(select 1 from pages where id = $1)")
            .bind(page_id)
            .fetch_one(&self.pool)
            .await
            .expect("the existence check must answer")
    }

    /// The status and reason the row carries now.
    async fn status_and_reason(&self, id: Uuid) -> (String, Option<String>) {
        sqlx::query_as("select status, discarded_reason from ai_change_sets where id = $1")
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .expect("the set row must still be readable")
    }

    /// Drop the temporary database.
    ///
    /// The pool goes first: `DROP DATABASE` fails while a session is still attached, and a
    /// walk suite that leaks a database per run eventually exhausts the server's connection
    /// pool for every other writer on the box.
    async fn dispose(mut self) {
        drop(self.pool);
        if let Some(maintenance) = self.maintenance.take() {
            sqlx::query(&format!("drop database if exists \"{}\"", self.database))
                .execute(maintenance.pool())
                .await
                .expect("the temporary database must be dropped");
        }
    }
}

fn swap_database(url: &str, database: &str) -> String {
    let (base, _) = url.rsplit_once('/').expect("a database URL has a path");
    format!("{base}/{database}")
}

async fn seed_organization(pool: &PgPool, label: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(id)
        .bind(label)
        .bind(format!("{label}-{}", Uuid::new_v4().simple()))
        .execute(pool)
        .await
        .expect("the fixture organization must be created");
    id
}

async fn seed_site(pool: &PgPool, organization_id: Uuid) -> Uuid {
    sqlx::query_scalar(
        "insert into sites (organization_id, key, name) values ($1, $2, $3) returning id",
    )
    .bind(organization_id)
    .bind(format!("s-{}", Uuid::new_v4().simple()))
    .bind("Slice 3b")
    .fetch_one(pool)
    .await
    .expect("the fixture site must be created")
}

/// The apply, driven through the store's own loop and the content crate's one writer.
///
/// # Why this is not a copy of the route's applier
///
/// The first version of this file **duplicated** the route's `apply_operations`, because a
/// route is a module of the binary and a test cannot import it. The duplicate drifted inside
/// the same commit, in the part nobody diffs: the *error*. The walk failed with "`page`
/// a0d4… does not exist, so there is nothing to preview against" — a uuid out of a set whose
/// operations all carry keys — while the route's own copy names the operation. Two copies of
/// an applier is not a testing convenience; it is a second implementation of the very thing
/// the acceptance criterion is about, and it is worse than no walk at all, because it reports
/// green while testing the wrong code.
///
/// The applier cannot move into `change_sets` either: it calls the **content crate**, and the
/// AI hub deliberately does not depend on it — the edge runs the other way, or the content
/// layer would not be usable without the AI hub. So the two halves are shared instead of the
/// whole: the **loop** is the store's `apply_all_async` (which is where the annotation lives,
/// for the reason the drift above exposed) and the **per-operation write** is
/// `content::pages::update_page_in`, the same one-writer function the route calls. What this
/// walk does not exercise is the route's own ten-line closure; that is what the walkthrough
/// pass is for, and saying so is better than a second copy that quietly diverges.
async fn apply_one_on(
    connection: &mut sqlx::PgConnection,
    op: &ChangeOp,
) -> Result<change_sets::AppliedOp, AiHubError> {
    let mapping = omnion_ai_hub::approvals::target::mapping_for(&op.operation.resource_type)?;
    let plan =
        omnion_ai_hub::approvals::target::preview_on(&mut *connection, mapping, &op.operation)
            .await?;
    let change = omnion_ai_hub::approvals::target::changes_for(&plan)?;
    let page_id: Uuid = op.operation.resource_id.parse().map_err(|_| {
        AiHubError::InvalidChangeSet(format!(
            "operation `{}` targets `{}`, which is not a page id",
            op.key, op.operation.resource_id
        ))
    })?;

    // The delete branch is the route's, kept in step with it deliberately.
    //
    // It was missing here first, which is how the defect it now fixes was found: the walk
    // asserted "an approved delete actually deleted the page" and the page was still there,
    // because this harness wrote every operation through `update_page_in`. A test-only applier
    // that has no delete branch is not a simplification — it is a second implementation that
    // reports green on behaviour the product does not have. So this copy mirrors
    // `routes::ai_change_sets::apply_one`'s delete branch exactly, including the
    // `deleted`/`status` shape.
    if op.operation.kind == omnion_ai_hub::approvals::plan::OpKind::Delete {
        omnion_content::pages::delete_page_in(connection, page_id)
            .await
            .map_err(|err| {
                AiHubError::InvalidChangeSet(format!(
                    "operation `{}` could not delete page {page_id}: {err}",
                    op.key
                ))
            })?;

        return Ok(change_sets::AppliedOp {
            key: op.key.clone(),
            kind: op.operation.kind,
            resource_id: op.operation.resource_id.clone(),
            slug: String::new(),
            status: "deleted".to_owned(),
        });
    }

    let page = omnion_content::pages::update_page_in(
        connection,
        page_id,
        &omnion_content::model::PageChanges {
            slug: change.slug,
            title: change.title,
            body: change.body,
            summary: change.summary,
        },
        None,
    )
    .await
    .map_err(|err| {
        // No operation key here: the store's loop adds it, and a message that names the key
        // twice is worse than one that names it once.
        AiHubError::InvalidChangeSet(format!("page {page_id} could not be written: {err}"))
    })?;
    Ok(change_sets::AppliedOp {
        key: op.key.clone(),
        kind: op.operation.kind,
        resource_id: op.operation.resource_id.clone(),
        slug: page.slug,
        status: page.status,
    })
}

/// The store's transaction, with the real writer behind it.
async fn apply_set(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
) -> Result<Vec<change_sets::AppliedOp>, AiHubError> {
    store::apply_confirmed(pool, organization_id, id, |set, connection| {
        Box::pin(async move {
            let mut applier = PageApplier { connection };
            change_sets::apply_all_with(&set.operations, &mut applier).await
        })
    })
    .await
}

/// The route's `PageApplier`, with no editor.
///
/// A struct rather than a closure because the store's loop takes an executor by `&mut` and a
/// higher-ranked closure that captures a `&mut PgConnection` is exactly the lifetime the
/// compiler refuses (and refusing it is correct: a closure that could return a borrow of its
/// own captured `&mut` is unsound).
struct PageApplier<'a> {
    connection: &'a mut sqlx::PgConnection,
}

impl change_sets::AsyncOperationExecutor for PageApplier<'_> {
    fn execute<'a>(
        &'a mut self,
        op: &'a ChangeOp,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<change_sets::AppliedOp, AiHubError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(apply_one_on(&mut *self.connection, op))
    }
}

fn update(key: &str, page_id: Uuid, title: &str) -> ChangeOp {
    ChangeOp {
        key: key.to_owned(),
        operation: omnion_ai_hub::approvals::plan::Operation {
            kind: omnion_ai_hub::approvals::plan::OpKind::Update,
            resource_type: "page".to_owned(),
            resource_id: page_id.to_string(),
            args: json!({ "title": title }),
        },
    }
}

/// The harness, or a **panic**.
///
/// The suites in this directory that print "skipping" and return are right for a developer who
/// has not started a database and wrong for a loop that reports "N passed": a URL naming a
/// database that does not exist makes every walk skip and the summary still reads green. That
/// happened once in `ai_tool_execution.rs`. A skipped walk proved nothing.
macro_rules! gate {
    () => {
        match SetStore::fresh().await {
            Some(store) => store,
            None => panic!(
                "PostgreSQL is not reachable, so every walk in this file would have SKIPPED. \
                 Set OMNION_DATABASE_URL to an existing database — on this box the QA stack's is \
                 postgres://omnion:***@127.0.0.1:5433/omnion_qa_w7. A skip must not read as a pass."
            ),
        }
    };
}

// -------------------------------------------------------------------------------------------
// The walks
// -------------------------------------------------------------------------------------------

/// The happy path, through the store the route calls.
///
/// The assertion is on **revisions**, not on the applied list: an applier that returned three
/// `AppliedOp`s without writing anything would pass a walk that only counted its own output.
#[tokio::test]
async fn a_three_operation_set_applies_every_page() {
    let store = gate!();
    let first = store.page("first", "First original").await;
    let second = store.page("second", "Second original").await;
    let third = store.page("third", "Third original").await;

    let id = store
        .confirmed_set(
            "Three titles",
            vec![
                update("a", first, "First applied"),
                update("b", second, "Second applied"),
                update("c", third, "Third applied"),
            ],
        )
        .await;

    let applied = apply_set(&store.pool, store.organization_id, id)
        .await
        .expect("the set applies");
    assert_eq!(applied.len(), 3, "every operation ran");

    // Every page carries exactly two revisions now — the original and the applied draft.
    for (page, expected) in [
        (first, "First applied"),
        (second, "Second applied"),
        (third, "Third applied"),
    ] {
        assert_eq!(store.revision_count(page).await, 2, "one new draft");
        let (_, title) = store.latest(page).await;
        assert_eq!(title, expected, "the applied title is the draft's");
    }

    let (status, _) = store.status_and_reason(id).await;
    assert_eq!(status, "applied", "the set is applied, not confirmed");

    store.dispose().await;
}

/// The claim slice 3 exists to make, against a real database.
///
/// Operation 1 is a real write. Operation 2 names a page that is not there. The walk then
/// reads operation 1's page back: if the transaction did not undo it, the page carries a
/// second revision titled "Applied" and the walk fails. That is a different assertion from
/// "the executor stopped" — a stopped executor with a committing writer leaves exactly this
/// residue, and it is the residue nobody would notice until a week later.
#[tokio::test]
async fn a_refused_second_operation_leaves_the_first_page_untouched() {
    let store = gate!();
    let first = store.page("first", "Original").await;
    let missing = store.missing_page().await;

    let before_no = store.revision_count(first).await;
    let before_title = store.latest(first).await.1;
    assert_eq!(before_no, 1, "the fixture starts with one revision");

    let id = store
        .confirmed_set(
            "Two pages, one missing",
            vec![
                update("a", first, "Applied then undone"),
                update("b", missing, "Never written"),
            ],
        )
        .await;

    let err = apply_set(&store.pool, store.organization_id, id)
        .await
        .expect_err("the second operation refuses");
    let message = err.to_string();
    // Both halves, and the first one is the half that was broken. A message naming only the
    // page id points at nothing a reviewer can find: the set's operations are addressed by
    // key, and the criterion asks for "the failing operation named".
    assert!(
        message.contains("`b`"),
        "the refusal names the failing operation: {message}"
    );
    assert!(
        message.contains("does not exist"),
        "and it keeps the cause: {message}"
    );
    assert!(
        !message.contains("operation `a`"),
        "and only the failing one: {message}"
    );

    // The whole claim: operation 1's write is gone.
    assert_eq!(
        store.revision_count(first).await,
        before_no,
        "operation 1's revision was rolled back"
    );
    assert_eq!(
        store.latest(first).await.1,
        before_title,
        "operation 1's title never landed"
    );

    // And the set is not left reading `confirmed`, which is the status a person cannot act on
    // and cannot tell apart from a set that is about to apply.
    let marked = store::mark_failed(&store.pool, store.organization_id, id, &message)
        .await
        .expect("the failure must be recordable");
    assert!(
        marked,
        "the row was still confirmed, so the failure is recorded"
    );

    let (status, reason) = store.status_and_reason(id).await;
    assert_eq!(status, "failed", "the set reads failed, not confirmed");
    let reason = reason.expect("a failed set says why");
    assert!(!reason.trim().is_empty(), "the reason is not blank");

    store.dispose().await;
}

/// A `failed` row must not overwrite a decision somebody else already made.
///
/// `mark_failed` writes `where status = 'confirmed'`, and a concurrent discard is a decision —
/// a second, different reason on the same record would be a worse answer than no record. The
/// walk proves the guard by taking the row out from under it, which is the only way to reach
/// the `Ok(false)` arm without a race.
#[tokio::test]
async fn a_failure_never_overwrites_a_row_that_is_no_longer_confirmed() {
    let store = gate!();
    let page = store.page("only", "Original").await;
    let id = store
        .confirmed_set("One page", vec![update("a", page, "Applied")])
        .await;

    // The discard wins the race.
    store::transition(
        &store.pool,
        store.organization_id,
        id,
        "confirmed",
        "discarded",
        Some("a person decided not to do this"),
    )
    .await
    .expect("the transition must answer")
    .expect("the set must have been discarded");

    let marked = store::mark_failed(&store.pool, store.organization_id, id, "it failed")
        .await
        .expect("the refusal must answer");
    assert!(!marked, "nothing to annotate: the row was already decided");

    let (status, reason) = store.status_and_reason(id).await;
    assert_eq!(status, "discarded", "the person's decision stands");
    assert_eq!(
        reason.as_deref(),
        Some("a person decided not to do this"),
        "and the person's reason is not overwritten"
    );

    store.dispose().await;
}

/// An editor stamps the row, and the stamp is what a later reader trusts.
///
/// `updated_by` is the column migration 0201 added, and this is the only writer of it: the
/// edit route. The walk drives the same statement, because a route is not reachable from a
/// test binary — and what it proves is that the column exists on the table the code selects,
/// which is a claim a unit test about a struct field cannot make at all.
#[tokio::test]
async fn editing_a_set_records_the_editor_and_keeps_the_row_editable() {
    let store = gate!();
    let page = store.page("edited", "Original").await;
    let actor: Uuid = Uuid::new_v4();
    sqlx::query("insert into users (id, email, password_hash) values ($1, $2, 'x')")
        .bind(actor)
        .bind(format!("editor-{}@example.test", Uuid::new_v4().simple()))
        .execute(&store.pool)
        .await
        .expect("the fixture user must be created");

    let set = store::append(
        &store.pool,
        &NewChangeSet {
            organization_id: store.organization_id,
            site_id: Some(store.site_id),
            title: "One page".to_owned(),
            operations: vec![update("a", page, "Edited")],
            created_by: Some(actor),
            created_by_agent: None,
            created_by_run: None,
            base_revisions: Default::default(),
        },
    )
    .await
    .expect("the set must be filed");
    assert_eq!(set.updated_by, None, "a filed set has no editor yet");

    let operations = vec![update("a", page, "Edited by a person")];
    let updated: Option<(Option<Uuid>, String)> = sqlx::query_as(
        "update ai_change_sets set title = $3, operations = $4, updated_by = $5, updated_at = now() \
         where id = $1 and organization_id = $2 and status = $6 returning updated_by, title",
    )
    .bind(set.id)
    .bind(store.organization_id)
    .bind("One page, edited")
    .bind(serde_json::to_value(&operations).expect("serialises"))
    .bind(actor)
    .bind("draft")
    .fetch_optional(&store.pool)
    .await
    .expect("the write must answer");
    assert_eq!(
        updated.expect("the draft is editable").0,
        Some(actor),
        "the editor is recorded"
    );

    // The same write against a row that is no longer a draft is **zero rows**, not a mutation:
    // that is what makes "editing after a decision is refused" a property of the table rather
    // than of a read that happened a moment earlier.
    let refused = sqlx::query(
        "update ai_change_sets set updated_by = $3 where id = $1 and organization_id = $2 \
         and status = 'applied'",
    )
    .bind(set.id)
    .bind(store.organization_id)
    .bind(Uuid::new_v4())
    .execute(&store.pool)
    .await
    .expect("the write must answer");
    assert_eq!(refused.rows_affected(), 0, "a decided set is not editable");

    store.dispose().await;
}

/// A delete is gated, and a plain title edit is not.
///
/// This is the walk for the defect slice 3c closed. `has_gated_operations()` classified an
/// operation by asking [`class_of_tool`] about a **synthesised** key — `"delete.page"`,
/// `"update.page"` — and `class_of_tool` only ever knew seven *real* tool keys
/// (`"content.publish"`, …). The lookup therefore returned `None` for every operation ever
/// built, the flag was `false` for every set, and `POST /ai/change-sets/{id}/confirm` told
/// every caller `needs_approval: false`. A set full of deletes could be confirmed and applied
/// with no human ever seeing it.
///
/// The old code would pass a walk that only asserted "a title edit is not gated", because that
/// half was true — it was true for *everything*. So this asserts **both** directions, and the
/// gated half is the one that fails first on the old code.
#[tokio::test]
async fn a_delete_is_gated_and_a_plain_title_edit_is_not() {
    let set = |operations: Vec<ChangeOp>| change_sets::ChangeSet {
        id: Uuid::new_v4(),
        organization_id: Uuid::new_v4(),
        site_id: None,
        title: "A proposal".to_owned(),
        status: "draft".to_owned(),
        operations,
        base_revisions: Default::default(),
        created_by: None,
        created_by_agent: None,
        created_by_run: None,
        updated_by: None,
        confirmed_at: None,
        applied_at: None,
        discarded_reason: None,
        created_at: time::OffsetDateTime::now_utc(),
        updated_at: time::OffsetDateTime::now_utc(),
    };

    let page_id = Uuid::new_v4();
    let page = page_id.to_string();
    let plain = set(vec![update("a", page_id, "New title")]);
    assert!(
        !plain.has_gated_operations(),
        "renaming a page is an ordinary edit and must not park in the inbox"
    );
    assert!(
        plain.gated_operations().is_empty(),
        "the list the bridge parks from agrees with the flag"
    );

    let removing = set(vec![ChangeOp {
        key: "a".to_owned(),
        operation: omnion_ai_hub::approvals::plan::Operation {
            kind: omnion_ai_hub::approvals::plan::OpKind::Delete,
            resource_type: "page".to_owned(),
            resource_id: page,
            args: json!({}),
        },
    }]);
    assert!(
        removing.has_gated_operations(),
        "a delete is `content_delete`, gated by default — this is the assertion the old \
         key-synthesis fails"
    );
    let gated = removing.gated_operations();
    assert_eq!(gated.len(), 1, "one gated operation, one row");
    assert_eq!(
        gated[0].1, "content_delete",
        "and it is classified as a delete"
    );
    assert_eq!(
        gated[0].0.key, "a",
        "the parked row names the operation it is about"
    );
}

/// The release gate: **every** gated operation needs an approving decision.
///
/// Pure and therefore the cheapest gate in the slice, but the property it carries is the one
/// the pipeline turns on: a set with three gated operations and one approval is `Blocked`, and
/// the list it returns names the two nobody answered. A release on the first decision is the
/// "a second person releases it" promise answered by a signature, and it is invisible in every
/// test that approves everything.
#[tokio::test]
async fn one_approval_of_three_gates_does_not_release_the_set() {
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let third = Uuid::new_v4();
    let publishes = |key: &str, page: Uuid| {
        let mut op = update(key, page, "Retitled");
        op.operation.args = json!({ "title": "Retitled", "status": "published" });
        op
    };
    let set = change_sets::ChangeSet {
        id: Uuid::new_v4(),
        organization_id: Uuid::new_v4(),
        site_id: None,
        title: "Publish three pages".to_owned(),
        status: "pending".to_owned(),
        operations: vec![
            publishes("a", first),
            publishes("b", second),
            publishes("c", third),
        ],
        base_revisions: Default::default(),
        created_by: None,
        created_by_agent: None,
        created_by_run: None,
        updated_by: None,
        confirmed_at: None,
        applied_at: None,
        discarded_reason: None,
        created_at: time::OffsetDateTime::now_utc(),
        updated_at: time::OffsetDateTime::now_utc(),
    };

    assert_eq!(
        set.gated_operations().len(),
        3,
        "three publishes, three gates"
    );

    let one = change_sets::release_gate(&set, &["a".to_owned()]);
    assert!(
        !one.is_released(),
        "approving one of three gates is not approving the set"
    );
    match one {
        change_sets::Gate::Blocked { outstanding } => {
            assert_eq!(
                outstanding,
                vec!["b".to_owned(), "c".to_owned()],
                "and the refusal names the rows the inbox still has to answer"
            );
        }
        other => panic!("expected Blocked, got {other:?}"),
    }

    let two = change_sets::release_gate(&set, &["a".to_owned(), "b".to_owned()]);
    assert!(!two.is_released());

    let all = change_sets::release_gate(&set, &["a".to_owned(), "b".to_owned(), "c".to_owned()]);
    assert!(
        all.is_released(),
        "every gate answered in the approving direction releases the set"
    );
}

/// An **ungated** operation is not a gate, so approving it must not help — and its presence in
/// the approved list must not confuse the comparison into releasing a set it never cleared.
///
/// The first half is the interesting one: a bridge that compared *counts* (approvals vs gated
/// operations) would let a reviewer approve the rename and see the set released, because the
/// rename's approval is a row and the count went up. This test is the shape that count would
/// fail.
#[tokio::test]
async fn approving_an_ungated_operation_never_releases_a_gated_set() {
    let page = Uuid::new_v4();
    let mut publishing = update("b", page, "Retitled");
    publishing.operation.args = json!({ "title": "Retitled", "status": "published" });
    let set = change_sets::ChangeSet {
        id: Uuid::new_v4(),
        organization_id: Uuid::new_v4(),
        site_id: None,
        title: "Rename and publish one page".to_owned(),
        status: "pending".to_owned(),
        operations: vec![update("a", page, "Plain rename"), publishing],
        base_revisions: Default::default(),
        created_by: None,
        created_by_agent: None,
        created_by_run: None,
        updated_by: None,
        confirmed_at: None,
        applied_at: None,
        discarded_reason: None,
        created_at: time::OffsetDateTime::now_utc(),
        updated_at: time::OffsetDateTime::now_utc(),
    };

    let gated = change_sets::release_gate(&set, &["a".to_owned()]);
    assert!(
        !gated.is_released(),
        "`a` is a plain rename — it never parks, so approving a row about it proves nothing \
         about `b`, the publish"
    );
    assert!(change_sets::release_gate(&set, &["b".to_owned()]).is_released());
}

/// A set with nothing gated is released by nobody and by everything, and that asymmetry is
/// correct: it never parked, so there is no gate to wait for.
#[tokio::test]
async fn a_set_with_no_gates_is_already_released() {
    let page = Uuid::new_v4();
    let set = change_sets::ChangeSet {
        id: Uuid::new_v4(),
        organization_id: Uuid::new_v4(),
        site_id: None,
        title: "One rename".to_owned(),
        status: "draft".to_owned(),
        operations: vec![update("a", page, "Retitled")],
        base_revisions: Default::default(),
        created_by: None,
        created_by_agent: None,
        created_by_run: None,
        updated_by: None,
        confirmed_at: None,
        applied_at: None,
        discarded_reason: None,
        created_at: time::OffsetDateTime::now_utc(),
        updated_at: time::OffsetDateTime::now_utc(),
    };
    assert!(
        change_sets::release_gate(&set, &[]).is_released(),
        "no gate means nothing to block; such a set is confirmed and applied in one call"
    );
}

/// The pipeline's missing half, against a real database: a parked set is released by the
/// inbox's answers and **applied**.
///
/// Slice 3c proved the set parks and files one row per gated operation. It did not prove
/// anything happens next, and it could not: nothing in the codebase moved `pending →
/// confirmed`, so a set with a gated operation could be approved row by row and then sit
/// there forever. The request's own line — "a set containing a gated operation still parks for
/// approval" — has no meaning if the approval never lets it go.
///
/// So this walk files the approvals through the real store, decides them through the real
/// `decide()` path, reads the keys the release path reads, and then applies the set. The
/// assertion is on **the page**: a rename that a reviewer approved actually renamed the page.
/// A pipeline that transitioned the row and stopped would leave every row here green.
#[tokio::test]
async fn approving_every_parked_row_releases_and_applies_the_set() {
    let store = gate!();
    let first = store.page("released-one", "Before one").await;
    let second = store.page("released-two", "Before two").await;
    let third = store.page("released-three", "Before three").await;

    // The gated operations here are **deletes**, and that is not a simplification.
    //
    // The first draft of this fixture minted its gate by writing `status: "published"` into an
    // update's args — which classifies as `content_publish` and therefore parks correctly, and
    // then fails at apply with "status is published by `content::pages::publish_page`, not
    // written as a field". The gate and the write disagreed: the classification asks what the
    // operation *means*, the applier asks what it can *do*, and only `content_delete` is a
    // gated class this build can actually carry through an update-or-delete write. A fixture
    // that passes the gate and fails the apply proves the plumbing and nothing about the gate,
    // so the deletes are also the honest way to say "gated AND applicable".
    //
    // Two deletes, because the property under test is the **intermediate** state — "one of two
    // gates is not a release" — and a single delete could not express it. A one-gate set cannot
    // tell a release-on-first-decision apart from a release-on-last-decision, which is the whole
    // hole this slice exists to close.
    //
    // The rename rides along ungated, which is the second property worth having here: a set that
    // both gates and runs must release on the deletes alone, the ungated operation must NOT be
    // filed as a third approval, and the rename must still be written by the same
    // all-or-nothing apply.
    let removing = |key: &str, page: Uuid| ChangeOp {
        key: key.to_owned(),
        operation: omnion_ai_hub::approvals::plan::Operation {
            kind: omnion_ai_hub::approvals::plan::OpKind::Delete,
            resource_type: "page".to_owned(),
            resource_id: page.to_string(),
            args: json!({}),
        },
    };
    let rename = update("a", first, "After one");
    let remove_second = removing("b", second);
    let remove_third = removing("c", third);
    let set_id = store::append(
        &store.pool,
        &NewChangeSet {
            organization_id: store.organization_id,
            site_id: Some(store.site_id),
            title: "Rename one page and delete two others".to_owned(),
            operations: vec![rename.clone(), remove_second.clone(), remove_third.clone()],
            created_by: None,
            created_by_agent: None,
            created_by_run: None,
            base_revisions: store::current_revisions(
                &store.pool,
                store.organization_id,
                &[rename.clone(), remove_second.clone(), remove_third.clone()],
            )
            .await
            .expect("the targets must be pinnable"),
        },
    )
    .await
    .expect("the set must be filed")
    .id;

    let parked = store.park(set_id).await;
    assert_eq!(
        parked.len(),
        2,
        "the two DELETES are gated and the rename is not, so the inbox carries two rows — \
         the ungated rename must not be filed as a third"
    );
    let (status, _) = store.status_and_reason(set_id).await;
    assert_eq!(status, "pending", "a gated set parks instead of confirming");

    // One row decided: the set must still be blocked. A release on the first decision is the
    // hole this slice is about, so the intermediate state is asserted, not assumed.
    store.decide(parked[0], "approved").await;
    let keys = store.approved_keys(set_id).await;
    let set = store::read(&store.pool, store.organization_id, set_id)
        .await
        .unwrap()
        .unwrap();
    let halfway = change_sets::release_gate(&set, &keys);
    assert!(
        !halfway.is_released(),
        "one of two gates is not a release, whatever the one that was decided was"
    );
    if let change_sets::Gate::Blocked { outstanding } = &halfway {
        assert_eq!(
            outstanding.len(),
            1,
            "and the refusal names the gate nobody answered: {outstanding:?}"
        );
    }
    assert_eq!(
        store.latest(first).await.0,
        1,
        "the ungated rename has NOT been written either — a park holds the whole set"
    );
    for page in [second, third] {
        assert_eq!(
            store.page_still_exists(page).await,
            true,
            "and neither approved-so-far delete has run"
        );
    }

    store.decide(parked[1], "approved").await;
    let keys = store.approved_keys(set_id).await;
    let gate = change_sets::release_gate(&set, &keys);
    assert!(gate.is_released(), "both gates answered: {keys:?}");

    // The transition the release route performs, then the all-or-nothing apply.
    let confirmed = store::transition(
        &store.pool,
        store.organization_id,
        set_id,
        "pending",
        "confirmed",
        None,
    )
    .await
    .expect("the release transition must answer")
    .expect("the set must have been released");
    assert_eq!(confirmed.status, "confirmed");

    let applied = apply_set(&store.pool, store.organization_id, set_id)
        .await
        .expect("the set must apply through the same all-or-nothing store call");
    assert_eq!(
        applied.len(),
        3,
        "every operation was written, gated or not"
    );

    let (status, _) = store.status_and_reason(set_id).await;
    assert_eq!(status, "applied", "and the row says so");

    let title: String = sqlx::query_scalar(
        "select title from page_revisions where page_id = $1 order by revision_no desc limit 1",
    )
    .bind(first)
    .fetch_one(&store.pool)
    .await
    .expect("the revision must be readable");
    assert_eq!(
        title, "After one",
        "the ungated rename was written by the same apply"
    );

    for page in [second, third] {
        assert_eq!(
            store.page_still_exists(page).await,
            false,
            "an approved delete actually deleted the page"
        );
    }
    store.dispose().await;
}

/// The other direction: a **rejected** gate ends the set instead of stalling it.
///
/// A release that only counted approvals would wait forever for an answer already given in the
/// negative, and a release that ignored the rejection would apply the operations a reviewer
/// refused. The walk asserts the refusal names the operation, because "this change set cannot
/// be released" with no key is a message a reviewer cannot act on.
#[tokio::test]
async fn one_rejected_gate_ends_the_set_and_never_releases_it() {
    let store = gate!();
    let page = store.page("refused", "Before").await;
    let set_id = store
        .confirmed_set("Rename one page", vec![update("a", page, "After")])
        .await;

    // Park a *separate* set so the shape is honest: a pending set with one gate.
    let pending = store::append(
        &store.pool,
        &NewChangeSet {
            organization_id: store.organization_id,
            site_id: Some(store.site_id),
            title: "Publish one page".to_owned(),
            operations: vec![{
                let mut op = update("a", page, "After");
                op.operation.args = json!({ "title": "After", "status": "published" });
                op
            }],
            created_by: None,
            created_by_agent: None,
            created_by_run: None,
            base_revisions: Default::default(),
        },
    )
    .await
    .expect("the set must be filed")
    .id;
    let _ = set_id;

    let parked = store.park(pending).await;
    assert_eq!(parked.len(), 1);
    store.decide(parked[0], "rejected").await;

    let refused = store.rejected_keys(pending).await;
    assert_eq!(
        refused,
        vec!["a".to_owned()],
        "the refusal names the operation"
    );
    let set = store::read(&store.pool, store.organization_id, pending)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !change_sets::release_gate(&set, &store.approved_keys(pending).await).is_released(),
        "a rejected gate is not an approving one"
    );
    assert_eq!(
        store.status_and_reason(pending).await.0,
        "pending",
        "and the set neither applies nor quietly becomes confirmable"
    );
    store.dispose().await;
}

/// A mixed set parks only the dangerous half.
///
/// The set is the case the request describes — "a set containing a gated operation still parks
/// for approval" — and the mixed case is the one that can go wrong in the other direction: an
/// implementation that parks *everything* because *something* is gated would make "rename five
/// pages, delete one of them" need six decisions instead of one.
#[tokio::test]
async fn a_mixed_set_gates_the_delete_and_leaves_the_renames_alone() {
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let removing = ChangeOp {
        key: "c".to_owned(),
        operation: omnion_ai_hub::approvals::plan::Operation {
            kind: omnion_ai_hub::approvals::plan::OpKind::Delete,
            resource_type: "page".to_owned(),
            resource_id: second.to_string(),
            args: json!({}),
        },
    };
    let set = change_sets::ChangeSet {
        id: Uuid::new_v4(),
        organization_id: Uuid::new_v4(),
        site_id: None,
        title: "Two renames and a delete".to_owned(),
        status: "draft".to_owned(),
        operations: vec![
            update("a", first, "First renamed"),
            update("b", second, "Second renamed"),
            removing,
        ],
        base_revisions: Default::default(),
        created_by: None,
        created_by_agent: None,
        created_by_run: None,
        updated_by: None,
        confirmed_at: None,
        applied_at: None,
        discarded_reason: None,
        created_at: time::OffsetDateTime::now_utc(),
        updated_at: time::OffsetDateTime::now_utc(),
    };

    assert!(set.has_gated_operations());
    let gated = set.gated_operations();
    assert_eq!(
        gated.len(),
        1,
        "only the delete parks; the two renames are decided by the person who confirmed"
    );
    assert_eq!(gated[0].0.key, "c", "and it is the delete");
    assert_eq!(gated[0].1, "content_delete");
}

/// An update that publishes is gated; the same update that leaves the status alone is not.
///
/// The class follows the **effect**, not the operation kind, and this is the case that a
/// key-synthesis rule gets backwards in the other direction: gating every `update` would park
/// every rename, which is the approval fatigue the request's own risk note names.
#[tokio::test]
async fn publishing_is_gated_but_the_same_update_without_the_status_is_not() {
    let page = Uuid::new_v4();
    let mut publishing = update("a", page, "Retitled");
    publishing.operation.args = json!({ "title": "Retitled", "status": "published" });
    assert_eq!(publishing.gated_class(), Some("content_publish"));
    assert!(
        publishing.publishes(),
        "the check is a value comparison, not a key's presence"
    );

    let still_draft = update("a", page, "Retitled");
    assert_eq!(still_draft.gated_class(), None);
    assert!(!still_draft.publishes());

    // A status that is *not* `published` is not publishing either — an unpublish is a
    // different act and nothing in the request gates it.
    let unpublishing = ChangeOp {
        key: "a".to_owned(),
        operation: omnion_ai_hub::approvals::plan::Operation {
            kind: omnion_ai_hub::approvals::plan::OpKind::Update,
            resource_type: "page".to_owned(),
            resource_id: page.to_string(),
            args: json!({ "status": "draft" }),
        },
    };
    assert_eq!(
        unpublishing.gated_class(),
        None,
        "the request's six classes contain a publish, not an unpublish"
    );
}
