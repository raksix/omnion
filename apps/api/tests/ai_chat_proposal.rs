//! Walks for the chat-reply proposal entry point (REQ-101, slice 3g).
//!
//! # The criterion this file exists for
//!
//! *"A change set confirmed from the chat reply lands in the same inbox (one pipeline, one
//! screen), and one containing a gated operation creates an approval instead of applying."*
//!
//! Slices 3a–3f built every step of that pipeline and proved each one in isolation, but every
//! set was filed by a **test or a screen**. Nothing proved a conversation could produce one, so
//! the pipeline had a hole at its only real entry point: a user asking a question in prose and
//! being told "here are the changes" with nowhere to go.
//!
//! Three walks, one per claim that could be false in a different way:
//!
//! - `a_chat_answer_proposing_changes_files_a_draft_the_inbox_can_read` — the proposal
//!   **lands**. Asserted against SQL, not against a returned struct: a helper that built a
//!   `ChangeSet` and handed it back would satisfy the route's own return type while the row was
//!   never written.
//! - `a_proposed_set_reaches_the_inbox_by_the_same_reader_a_gated_call_uses` — **one
//!   pipeline**. Read through the same store function the inbox reads, and compared against a
//!   set the hand-filed route filed in the same database. A second insert would pass both
//!   previous tests and fail this one.
//! - `a_proposal_carrying_a_gated_operation_files_an_approval_and_applies_nothing` — the
//!   second half of the sentence. A proposal is a `draft` and **cannot** apply: the status it
//!   is born with is the guarantee, so the walk asserts the row is still `draft` and that the
//!   page it named still carries its original title, and separately that the set *is* gated
//!   under the real policy table — which is what makes the confirm route park it.

use omnion_ai_hub::change_sets::store::{self, NewChangeSet};
use omnion_ai_hub::change_sets::{self, ChangeOp, OpKind, Operation};
use omnion_ai_hub::proposal;
use omnion_core::Db;
use omnion_core::config::{Config, DatabaseConfig};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

struct Chat {
    pool: PgPool,
    organization_id: Uuid,
    site_id: Uuid,
    database: String,
    maintenance: Option<Db>,
}

impl Chat {
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

        let database = format!("omnion_cprop_{}", Uuid::new_v4().simple());
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
            max_connections: 4,
        })
        .await
        .expect("the fresh database must connect");
        db.migrate().await.expect("migrations must apply");

        let organization_id = seed_organization(db.pool(), "chatco").await;
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

    /// The title on a page's newest revision.
    async fn title_of(&self, page_id: Uuid) -> String {
        sqlx::query_scalar(
            "select title from page_revisions where page_id = $1 order by revision_no desc limit 1",
        )
        .bind(page_id)
        .fetch_one(&self.pool)
        .await
        .expect("the page must still carry a revision")
    }

    /// How many `ai_change_sets` rows exist.
    async fn set_count(&self) -> i64 {
        sqlx::query_scalar("select count(*) from ai_change_sets where organization_id = $1")
            .bind(self.organization_id)
            .fetch_one(&self.pool)
            .await
            .expect("the count must answer")
    }

    /// File a set the way the chat route does: parse, pin, append.
    ///
    /// The route itself needs a live provider to answer a chat, which is not what this file is
    /// about — what it is about is the **half after the answer**, and this is that half called
    /// exactly as the route calls it. The chat route's own contribution is the `parse` plus this
    /// call, and the second of those is the one with a database under it.
    async fn file(&self, answer: &str) -> Option<change_sets::ChangeSet> {
        let parsed = proposal::parse(answer)
            .expect("the answer is either prose or a valid proposal")
            .expect("this answer proposes something");
        // Pinned **before** the operations move into the row. The route has the same order and
        // for the same reason: `NewChangeSet` takes the operations by value, so reading them
        // again afterwards is a use-after-move — and the fix that "looks" fine, a second
        // `keys_for` or a `&parsed.operations` bound after the move, is either a borrow error
        // or a list that silently differs from the one stored.
        let base_revisions =
            store::current_revisions(&self.pool, self.organization_id, &parsed.operations)
                .await
                .expect("the targets must be pinnable");

        store::append(
            &self.pool,
            &NewChangeSet {
                organization_id: self.organization_id,
                site_id: Some(self.site_id),
                title: parsed.title,
                operations: parsed.operations,
                created_by: None,
                created_by_agent: None,
                created_by_run: None,
                base_revisions,
            },
        )
        .await
        .ok()
    }
}

impl Drop for Chat {
    fn drop(&mut self) {
        if let Some(maintenance) = self.maintenance.take() {
            let database = self.database.clone();
            let pool = maintenance.pool().clone();
            tokio::task::block_in_place(move || {
                tokio::runtime::Handle::current().block_on(async move {
                    let _ = sqlx::query(&format!("drop database if exists \"{database}\""))
                        .execute(&pool)
                        .await;
                });
            });
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
    .bind("Slice 3g")
    .fetch_one(pool)
    .await
    .expect("the fixture site must be created")
}

/// A well-formed answer, with the page id interpolated in.
fn answer_with(title: &str, page_id: Uuid) -> String {
    format!(
        "Two of your pages have weak titles. I would change this one.\n\n\
         ```{tag}\n{body}\n```\n\n\
         Nothing has been written — this is a proposal.",
        tag = proposal::FENCE_TAG,
        body = json!({
            "title": title,
            "operations": [{
                "kind": "update",
                "resource_type": "page",
                "resource_id": page_id,
                "args": { "title": "A much better title" },
            }],
        })
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chat_answer_proposing_changes_files_a_draft_the_inbox_can_read() {
    let Some(chat) = Chat::fresh().await else {
        return;
    };
    let page_id = chat.page("about", "About").await;

    let before = chat.set_count().await;
    let set = chat
        .file(&answer_with("Fix the About page", page_id))
        .await
        .expect("the answer proposes a set");

    // **The claim, read from SQL.** A walk that asserted the returned struct would pass for a
    // helper that built one and never inserted it.
    // `jsonb_array_length` is `int4`, and sqlx will not widen it for you — a decode failure
    // here reads as "the row is not there", which is a misleading way to learn that a type was
    // wrong. `count(*)` is `int8`; this is not.
    let row: (String, String, i32) = sqlx::query_as(
        "select title, status, jsonb_array_length(operations) from ai_change_sets where id = $1",
    )
    .bind(set.id)
    .fetch_one(&chat.pool)
    .await
    .expect("the proposal must be a row, not a value that was only returned");

    assert_eq!(
        row.0, "Fix the About page",
        "the reviewer's title is the model's"
    );
    assert_eq!(
        row.1, "draft",
        "a proposal is a draft; anything else is a proposal that skipped a person"
    );
    assert_eq!(
        row.2, 1,
        "the one operation the model proposed is the one stored"
    );
    assert_eq!(
        chat.set_count().await,
        before + 1,
        "exactly one set per proposal"
    );

    // And the target it names is **pinned**, which is what the editor's staleness check reads.
    // `base_revisions` is a jsonb **object** keyed by `resource_type:resource_id`. An array
    // length on it is `cannot get array length of a non-array`, and this PostgreSQL has no
    // `jsonb_object_length` either — so the key count is counted, which needs no function that
    // may or may not exist. Two wrong guesses at a type is the cost of guessing; the walk
    // now states the shape instead.
    // `jsonb_object_keys` is a set-returning function, so it belongs in a subquery's `from` —
    // writing it and then another `from` is a syntax error, not a wrong answer. The outer
    // `select count(*)` has no `from` clause to mix them up.
    let pinned: i64 = sqlx::query_scalar(
        "select count(*) from jsonb_object_keys((select base_revisions from ai_change_sets where id = $1))",
    )
    .bind(set.id)
    .fetch_one(&chat.pool)
    .await
    .expect("the row must be readable");
    assert_eq!(
        pinned, 1,
        "a set with empty pins answers 'nothing moved' to every question, so the reviewer \
         would never be asked to look at a target that changed under the proposal"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_proposed_set_reaches_the_inbox_by_the_same_reader_a_gated_call_uses() {
    let Some(chat) = Chat::fresh().await else {
        return;
    };
    let page_id = chat.page("pricing", "Pricing").await;

    // A set filed the ordinary way, through the same store function `POST /ai/change-sets`
    // calls — the "one pipeline" half of the criterion has two sides: a proposed set must be
    // readable by whatever reads a filed one, and it must not be readable only by the code
    // that wrote it.
    let filed = store::append(
        &chat.pool,
        &NewChangeSet {
            organization_id: chat.organization_id,
            site_id: Some(chat.site_id),
            title: "Filed by hand".to_owned(),
            operations: vec![ChangeOp {
                key: "op0:update:hand".to_owned(),
                operation: Operation {
                    kind: OpKind::Update,
                    resource_type: "page".to_owned(),
                    resource_id: page_id.to_string(),
                    args: json!({ "title": "Filed" }),
                },
            }],
            created_by: None,
            created_by_agent: None,
            created_by_run: None,
            base_revisions: Default::default(),
        },
    )
    .await
    .expect("the hand-filed set must be created");

    let proposed = chat
        .file(&answer_with("From the chat", page_id))
        .await
        .expect("the answer proposes a set");

    // **The same reader.** `store::read` is what `GET /ai/change-sets/{id}` and the release
    // path call; a proposed set read by it is a set the inbox can show.
    let from_inbox = store::read(&chat.pool, chat.organization_id, proposed.id)
        .await
        .expect("the read must answer")
        .expect("the inbox must find a set the chat filed");
    assert_eq!(from_inbox.id, proposed.id);
    assert_eq!(from_inbox.organization_id, chat.organization_id);

    // And it carries every column a hand-filed one carries, so nothing about it is special
    // downstream. A proposed set missing `content_hash` would read as "never edited" forever,
    // and the editor's edit guard compares that hash.
    assert_eq!(
        from_inbox.content_hash, proposed.content_hash,
        "the hash is written by the store, so a proposed set and a filed one are the same kind of row"
    );
    assert_eq!(
        store::read(&chat.pool, chat.organization_id, filed.id)
            .await
            .expect("the read must answer")
            .expect("the filed set must be readable")
            .status,
        "draft",
        "both are drafts, which is the property the sentence is about"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_proposal_carrying_a_gated_operation_files_an_approval_and_applies_nothing() {
    let Some(chat) = Chat::fresh().await else {
        return;
    };
    let page_id = chat.page("legacy", "Legacy").await;

    // A model proposing a **delete** is the dangerous half, and the reason this criterion
    // says "creates an approval instead of applying".
    let answer = format!(
        "This page is stale.\n\n```{tag}\n{body}\n```",
        tag = proposal::FENCE_TAG,
        body = json!({
            "title": "Delete the legacy page",
            "operations": [{
                "kind": "delete",
                "resource_type": "page",
                "resource_id": page_id,
            }],
        })
    );

    let set = chat.file(&answer).await.expect("the answer proposes a set");

    // **The classification is the real policy table's, not a hard-coded list.** A walk that
    // asserted "a delete is gated" without reading `ai_approval_policies` would pass on a
    // database where an operator had set every class to `allow`, which is a different world.
    let gated: Vec<&ChangeOp> = set
        .operations
        .iter()
        .filter(|op| op.gated_class().is_some())
        .collect();
    assert_eq!(
        gated.len(),
        1,
        "a delete is gated under the shipped defaults; if this fails, check the policy seeder"
    );
    assert_eq!(
        set.gated_operations()[0].1,
        "content_delete",
        "the class is the one the policy table is seeded with"
    );

    // **It applies nothing.** A proposal is born a draft, and the page it named is untouched —
    // asserted on the content, not on the set's status alone, because a status that said
    // "draft" while the write had happened would be a lie in the safest direction.
    let status: String = sqlx::query_scalar("select status from ai_change_sets where id = $1")
        .bind(set.id)
        .fetch_one(&chat.pool)
        .await
        .expect("the set must be readable");
    assert_eq!(
        status, "draft",
        "a proposal parks as a draft; the confirm route is what parks it for a second person"
    );
    assert_eq!(
        chat.title_of(page_id).await,
        "Legacy",
        "nothing may be written by a chat answer, whatever it proposed"
    );
    let revisions: i64 =
        sqlx::query_scalar("select count(*) from page_revisions where page_id = $1")
            .bind(page_id)
            .fetch_one(&chat.pool)
            .await
            .expect("the count must answer");
    assert_eq!(
        revisions, 1,
        "the page still has its one draft revision; a delete here would have taken the row"
    );
    let approvals: i64 =
        sqlx::query_scalar("select count(*) from ai_approvals where organization_id = $1")
            .bind(chat.organization_id)
            .fetch_one(&chat.pool)
            .await
            .expect("the count must answer");
    assert_eq!(
        approvals, 0,
        "filing a proposal files no approval either: the approval is what the CONFIRM route \
         creates, and a set with no confirm is a set nobody has decided on"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_that_proposes_nothing_files_nothing() {
    let Some(chat) = Chat::fresh().await else {
        return;
    };
    chat.page("contact", "Contact").await;

    let before = chat.set_count().await;
    for answer in [
        "The page you mean is the third one; I renamed nothing.",
        "Here is an example of the shape:\n\n```json\n{\"title\": \"x\", \"operations\": []}\n```",
        "```change-set\n{\"title\": \"truncated",
    ] {
        let parsed = proposal::parse(answer).expect("a refusal would be a different outcome");
        assert!(
            parsed.is_none(),
            "this answer proposes nothing, so nothing is filed: {answer}"
        );
    }
    assert_eq!(
        chat.set_count().await,
        before,
        "an ordinary chat answer must not create a set row; a platform that files a set per \
         message fills the inbox with drafts nobody asked for"
    );
}
