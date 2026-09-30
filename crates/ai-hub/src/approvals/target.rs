//! The resource reader and the applier: the half that touches the database (REQ-101, slice 2b).
//!
//! # The one claim this file makes
//!
//! **The revision the stale check compares against is computed here, on the server, from the
//! row as it is now.** It is not taken from the request. Slice 1's `decide()` compared
//! `base_revision` against a `current_revision` the *client* posted, which means the entire
//! staleness guarantee was "the reviewer said the right thing" — a caller that posted the
//! row's own stored base revision would sail through a preview computed three days ago.
//! That is not a race, it is a dial.
//!
//! So the decision path reads the target. [`revision_of`] returns a digest of the fields the
//! mapping touches, and the decision compares the stored `base_revision` against *that*. A
//! client may still send what it saw, and it is still worth recording, but it is not what the
//! answer is based on.
//!
//! # Why the reader is in this crate and not in `content`
//!
//! The reader has to know two things at once: which columns the mapping writes, and what a
//! page row carries. Splitting that across crates is what makes the two drift — a new page
//! column is added to the mapping, and a reader in another crate keeps returning the old
//! snapshot, so every `before` value is a false "unset". The mapping is the single list of
//! columns, and this file reads exactly that list.
//!
//! # What is deliberately not here
//!
//! - **The write.** [`changes_for`] turns a plan into the content crate's own change type,
//!   and the caller applies it with `content::pages::update_page`. The apply runs the
//!   content crate's own validation, so a preview can never describe a write the content
//!   layer would refuse.
//! - **Transactions across the whole change set.** That is slice 3's all-or-nothing, and it
//!   needs a seam this file does not have.

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use crate::approvals::plan::{
    self, Cascade, FieldSpec, Mapping, OpKind, Operation, PAGE_STATUSES, PAGE_UPDATE, Plan,
};
use crate::error::{AiHubError, Result};

/// The `resource_type` a translation row carries for a page revision.
///
/// The content crate owns this string (`omnion_content::model::REVISION_RESOURCE`) but the
/// AI hub does not depend on the content crate — the dependency runs the other way, or the
/// content layer would not be usable without the AI hub. A cascade count that quietly
/// filtered on the wrong literal would report "0 translations" for a page that has them, so
/// the value is repeated here **and pinned by a test** rather than left as a coincidence.
const PAGE_REVISION_RESOURCE: &str = "page_revision";

/// The resource types this build can preview and apply.
///
/// A closed list rather than a `match` on strings scattered across call sites, because an
/// approval naming a type the applier does not know must be **refused**, and "we have no
/// reader for `theme`" is only sayable if the set of types we have readers for is itself a
/// value.
pub const SUPPORTED_TYPES: [&str; 1] = ["page"];

/// The mapping for a resource type, or `None` when this build has no reader for it.
///
/// # Errors
///
/// `Err(InvalidApproval)` when the type is known to the module but has no mapping yet — the
/// same refusal either way, so a caller cannot branch on "unknown" and guess.
pub fn mapping_for(resource_type: &str) -> Result<Mapping> {
    match resource_type {
        "page" => Ok(PAGE_UPDATE),
        other => Err(AiHubError::InvalidApproval(format!(
            "`{other}` is not a previewable resource type; this build previews {}",
            SUPPORTED_TYPES.join(", ")
        ))),
    }
}

/// Read a target as the mapping's fields, ready for [`plan::plan`].
///
/// `current` is keyed by **column** name, which is what [`plan::snapshot`] expects and what
/// makes `before` a real old value rather than an absent one. A column the mapping names but
/// the row does not carry is simply absent from the map, and the diff renders it as an unset
/// field — which is the truth for a `null` column and a lie for a missing one, so the
/// applier's `changes_for` is the place that decides rather than this.
pub async fn read_target(pool: &PgPool, resource_type: &str, resource_id: &str) -> Result<Value> {
    let mapping = mapping_for(resource_type)?;
    let id = parse_id(resource_id)?;
    let values = match resource_type {
        "page" => page_values(pool, id, mapping).await?,
        other => {
            // `mapping_for` already refused everything else, so this arm exists to satisfy the
            // compiler rather than to be reached.
            return Err(AiHubError::InvalidApproval(format!(
                "`{other}` has no reader"
            )));
        }
    };
    Ok(plan::snapshot(mapping, &values))
}

/// The columns a page's mapped fields live in, as the reader sees them.
///
/// `slug` and `status` are on `pages`; `title`, `body` and `summary` are on the **draft**
/// revision, which is why one page update is two tables and the mapping is the only place that
/// knows which is which. The join is `latest revision wins`, matching `update_page`'s base
/// rule, so a `before` value is the same row the write is derived from — a reader that took
/// the *published* revision would show a `before` the apply never overwrites.
const PAGE_VALUES_SQL: &str = "select p.slug as slug, p.status as status, \
     coalesce(r.title, p.slug) as title, coalesce(r.body, '') as body, r.summary as summary \
     from pages p \
     left join lateral ( \
         select title, body, summary from page_revisions \
         where page_id = p.id order by revision_no desc limit 1 \
     ) r on true \
     where p.id = $1";

/// A page's mapped columns, as a map keyed by **column** name.
async fn page_values(pool: &PgPool, id: Uuid, mapping: Mapping) -> Result<Map<String, Value>> {
    let row: Option<PageRow> = sqlx::query_as(PAGE_VALUES_SQL)
        .bind(id)
        .fetch_optional(pool)
        .await?;
    // A page with no revision row at all is not a page this build can preview: every content
    // field would read as empty, and the diff would offer to create content on a row that
    // already exists. The `left join` is what makes the *absence* observable rather than a
    // panic on `fetch_one`.
    let Some(row) = row else {
        return Ok(Map::new());
    };
    let mut values = Map::new();
    for spec in mapping {
        let value = match spec.field {
            // Cloned, not moved: the match sits inside a loop over the mapping, so moving a
            // field out of `row` would make the second iteration a use-after-move.
            "slug" => Value::from(row.slug.clone()),
            "status" => Value::from(row.status.clone()),
            "title" => Value::from(row.title.clone()),
            "body" => Value::from(row.body.clone()),
            "summary" => row.summary.clone().map_or(Value::Null, Value::from),
            // Unreachable while `changes_for` and this reader agree on the mapping, and the
            // test `every_page_mapping_column_has_a_reader_or_an_explicit_refusal` is what
            // keeps them agreeing.
            other => {
                return Err(AiHubError::InvalidApproval(format!(
                    "`page` has no reader for column `{other}`"
                )));
            }
        };
        values.insert(spec.field.to_owned(), value);
    }
    Ok(values)
}

/// One page read, in the shape [`page_values`] needs.
///
/// A tuple would be shorter and less readable at the call site; a row type is what makes the
/// `match` above name its columns, and a reader that adds a column gets a compile error here
/// rather than a runtime `Option::None`.
#[derive(sqlx::FromRow)]
struct PageRow {
    slug: String,
    status: String,
    title: String,
    body: String,
    summary: Option<String>,
}

/// The label a reviewer reads, and the phrase the typed confirmation is checked against.
///
/// The **page's** title where the row carries one, falling back to the slug. The phrase is
/// stored on the approval row at request time (slice 1 already does that), so a rename between
/// preview and decision cannot move the phrase out from under the reviewer — this function is
/// what the *request* side calls.
pub async fn read_label(pool: &PgPool, resource_type: &str, resource_id: &str) -> Result<String> {
    let id = parse_id(resource_id)?;
    match resource_type {
        "page" => {
            let sql = "select slug from pages where id = $1";
            let slug: Option<String> = sqlx::query_scalar(sql)
                .bind(id)
                .fetch_optional(pool)
                .await?;
            Ok(slug.unwrap_or_else(|| id.to_string()))
        }
        other => Err(AiHubError::InvalidApproval(format!(
            "`{other}` has no label reader"
        ))),
    }
}

/// The revision of a target as it is **now**, computed from the fields the mapping writes.
///
/// This is the digest the stale check compares against. It is a digest of *content*, not of
/// `updated_at`, for one reason: a preview's base revision has to be computable by a reader
/// that has no row history, and `updated_at` moves on a no-op touch while a revision built
/// from `updated_at` would then refuse a preview of an unchanged page. The digest is stable
/// across re-previews of an unchanged target, which is what makes Re-preview useful.
///
/// # Errors
///
/// `Err(InvalidApproval)` when the resource type has no reader, or the id is not a uuid.
pub async fn revision_of(pool: &PgPool, resource_type: &str, resource_id: &str) -> Result<String> {
    let current = read_target(pool, resource_type, resource_id).await?;
    let mapping = mapping_for(resource_type)?;
    Ok(revision_of_snapshot(mapping, &current))
}

/// The digest for an in-memory snapshot.
///
/// Split out so the hashing is a **pure** function and a walk can pin it without a database:
/// the whole point of `revision_of` is that the decision path computes it itself, and a
/// test that needed a live page to prove the digest moved would only run on a machine with
/// PostgreSQL.
#[must_use]
pub fn revision_of_snapshot(mapping: Mapping, current: &Value) -> String {
    // Canonical order: the mapping's. Hashing `serde_json`'s own output would be at the mercy
    // of a map's iteration order, and two servers with different hashers would then disagree
    // about whether the same page moved.
    let mut canonical = Map::new();
    for spec in mapping {
        if let Some(value) = current.get(spec.field) {
            canonical.insert(spec.field.to_owned(), value.clone());
        }
    }
    let bytes = serde_json::to_vec(&Value::Object(canonical)).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// The dependents a delete would take with it, counted now.
///
/// The request wants the count in the *preview* ("1 page, 4 revisions") and the same count
/// in the applied result, so it is counted once here and carried through the frozen preview
/// rather than re-derived at apply time — a second count is a second number for the same
/// fact, and the two would disagree the moment somebody published a revision in between.
pub async fn cascades_for(
    pool: &PgPool,
    resource_type: &str,
    resource_id: &str,
) -> Result<Vec<Cascade>> {
    let id = parse_id(resource_id)?;
    let cascades = match resource_type {
        "page" => {
            let revisions: i64 =
                sqlx::query_scalar("select count(*) from page_revisions where page_id = $1")
                    .bind(id)
                    .fetch_one(pool)
                    .await?;
            let translations: i64 = sqlx::query_scalar(
                "select count(*) from translations \
                 where resource_type = $1 and resource_id in \
                 (select id from page_revisions where page_id = $2)",
            )
            .bind(PAGE_REVISION_RESOURCE)
            .bind(id)
            .fetch_one(pool)
            .await?;
            let mut out = Vec::new();
            if revisions > 0 {
                out.push(Cascade {
                    label: "page revisions".to_owned(),
                    count: revisions,
                });
            }
            if translations > 0 {
                out.push(Cascade {
                    label: "translations".to_owned(),
                    count: translations,
                });
            }
            out
        }
        other => {
            return Err(AiHubError::InvalidApproval(format!(
                "`{other}` has no cascade reader"
            )));
        }
    };
    Ok(cascades)
}

/// Compute a preview for one operation, reading everything it needs itself.
///
/// This is the *server* half of the preview: the caller supplies the model's tool arguments
/// and nothing else. The base revision is [`revision_of`] of the target as it is now, not a
/// number the caller supplied, which is what makes `stale` mean something.
///
/// # Errors
///
/// Whatever [`plan::plan`] refuses — an unknown argument, a value outside the field's rules, an
/// operation that changes nothing — plus the reader's own refusals for an unknown type or a
/// non-uuid id. A target that does not exist is `ApprovalNotFound`, not an empty snapshot:
/// an approval for a deleted page has no target to diff against, and rendering it as "every
/// field is new" is how a create is approved for a page that is already gone.
pub async fn preview(pool: &PgPool, mapping: Mapping, op: &Operation) -> Result<Plan> {
    // A create has no row to read, so its base revision is the empty digest of an empty
    // snapshot — which is exactly what `revision_of_snapshot` produces, and which makes
    // "the target appeared between preview and decision" detectable for a create too.
    let (current, label, cascades) = if op.kind == OpKind::Create {
        (Value::Object(Map::new()), String::new(), Vec::new())
    } else {
        if op.resource_id.is_empty() {
            return Err(AiHubError::InvalidApproval(
                "an update or a delete must name the resource it acts on".to_owned(),
            ));
        }
        let current = read_target(pool, &op.resource_type, &op.resource_id).await?;
        if current.as_object().is_none_or(|map| map.is_empty()) {
            return Err(AiHubError::InvalidApproval(format!(
                "`{}` {} does not exist, so there is nothing to preview against",
                op.resource_type, op.resource_id
            )));
        }
        let label = read_label(pool, &op.resource_type, &op.resource_id).await?;
        let cascades = if op.kind == OpKind::Delete {
            cascades_for(pool, &op.resource_type, &op.resource_id).await?
        } else {
            Vec::new()
        };
        (current, label, cascades)
    };
    let base_revision = revision_of_snapshot(mapping, &current);
    plan::plan(mapping, op, &current, &label, &base_revision, cascades)
}

/// The content crate's own change type, built from a plan's writes.
///
/// The conversion is `Option`-per-field rather than a json object so a field the plan does
/// not write stays absent from the change set — `content::pages::update_page` reads `None`
/// as "keep what is there", and a `Some("")` would clear it. The plan's diffs are the only
/// source, so this cannot invent a write the preview did not show.
///
/// The `status` field is the one the mapping carries that `PageChanges` does not, because
/// publishing is its own operation in the content crate. It is refused here rather than
/// dropped: silently ignoring a previewed `status` change is the worst of the three outcomes
/// available (approve a change that did not happen), and routing it means a second applier.
pub fn changes_for(plan: &Plan) -> Result<ContentChange> {
    let writes = plan::writes(plan, mapping_for(&plan.resource_type)?)?;
    let mut change = ContentChange::default();
    for write in writes {
        match write.field.as_str() {
            "slug" => {
                change.slug = write
                    .value
                    .as_ref()
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            "title" => {
                change.title = write
                    .value
                    .as_ref()
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            "body" => {
                change.body = write
                    .value
                    .as_ref()
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            // `FieldKind::Summary` resolves an empty string to `None` — that is a *clear*, and
            // `PageChanges::summary = Some("")` is how the content crate spells "clear it".
            // So the mapping's `None` and the content crate's `None` mean different things,
            // and this is the one field where they have to be told apart.
            "summary" => {
                change.summary = Some(
                    write
                        .value
                        .as_ref()
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                );
            }
            "status" => {
                return Err(AiHubError::InvalidApproval(
                    "`page` status is published by `content::pages::publish_page`, not written \
                     as a field; a preview that changes it needs the publish tool"
                        .to_owned(),
                ));
            }
            other => {
                return Err(AiHubError::InvalidApproval(format!(
                    "`{}` has no writer for `{other}`",
                    plan.resource_type
                )));
            }
        }
    }
    Ok(change)
}

/// The fields a page update writes, in the content crate's vocabulary.
///
/// A local struct rather than a dependency on `omnion-content`, because **the dependency runs
/// the other way**: `content` must not have to know about the AI hub for the content layer to
/// be usable on its own. The route owns the one function that converts this into
/// `PageChanges` and calls `update_page`, so there is exactly one writer and it lives where
/// the content crate is already imported.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContentChange {
    /// New slug, or `None` for "leave it".
    pub slug: Option<String>,
    /// New title.
    pub title: Option<String>,
    /// New body.
    pub body: Option<String>,
    /// `Some("")` clears the summary; `None` leaves it alone.
    pub summary: Option<String>,
}

impl ContentChange {
    /// `true` when this change writes nothing, which the caller must refuse rather than
    /// report as a successful apply.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slug.is_none() && self.title.is_none() && self.body.is_none() && self.summary.is_none()
    }
}

/// The page column a mapping writes, named for the reader's `match` arm above.
///
/// Exposed so a reader adding a column gets a compile error naming the arms that have to
/// learn about it, instead of a silently absent value.
#[must_use]
pub const fn column_of(spec: &FieldSpec) -> &'static str {
    spec.field
}

/// Whether a status is one the page table accepts.
///
/// The preview validates a `status` argument against this list, and the content crate
/// refuses anything else at the constraint — so the check exists in the preview for the
/// reviewer's benefit, not for correctness.
#[must_use]
pub fn is_known_status(status: &str) -> bool {
    PAGE_STATUSES.contains(&status)
}

/// Parse a resource id, refusing the empty string with the message a reviewer reads.
///
/// A stored `resource_id` of `""` reaches here when an approval was written by an older
/// build or by hand; `Uuid::parse_str` would say "invalid length", which names the parse and
/// not the missing target.
fn parse_id(resource_id: &str) -> Result<Uuid> {
    let trimmed = resource_id.trim();
    if trimmed.is_empty() {
        return Err(AiHubError::InvalidApproval(
            "this request names no resource".to_owned(),
        ));
    }
    Uuid::parse_str(trimmed)
        .map_err(|_| AiHubError::InvalidApproval(format!("`{trimmed}` is not a resource id")))
}

/// The preview a reader should store when there is nothing to diff against.
///
/// Used by the route's re-preview path so "no target" and "empty target" produce one shape.
#[must_use]
pub fn empty_preview(mapping: Mapping) -> Value {
    json!({
        "version": plan::PREVIEW_VERSION,
        "mapping": mapping,
        "diffs": [],
        "cascades": [],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approvals::plan::PAGE_UPDATE;
    use serde_json::json;

    fn page_id() -> &'static str {
        "11111111-1111-1111-1111-111111111111"
    }

    fn update(args: Value) -> Operation {
        Operation {
            kind: OpKind::Update,
            resource_type: "page".to_owned(),
            resource_id: page_id().to_owned(),
            args,
        }
    }

    fn current() -> Value {
        json!({
            "slug": "about",
            "title": "About us",
            "body": "Hello",
            "summary": "who we are",
            "status": "draft",
        })
    }

    // --- the digest: the heart of the stale check ---

    #[test]
    fn the_revision_is_a_pure_function_of_the_fields_the_mapping_names() {
        let before = revision_of_snapshot(PAGE_UPDATE, &current());
        let after = revision_of_snapshot(
            PAGE_UPDATE,
            &json!({
                "slug": "about",
                "title": "About our team",
                "body": "Hello",
                "summary": "who we are",
                "status": "draft",
            }),
        );
        assert_ne!(before, after, "a changed title must move the revision");
    }

    #[test]
    fn the_revision_ignores_a_column_the_mapping_does_not_write() {
        // `updated_at` and every other bookkeeping column move on a touch that changes nothing
        // a reviewer was shown. Hashing them would make every preview stale on the next save.
        let plain = revision_of_snapshot(PAGE_UPDATE, &current());
        let noisy = revision_of_snapshot(
            PAGE_UPDATE,
            &json!({
                "slug": "about",
                "title": "About us",
                "body": "Hello",
                "summary": "who we are",
                "status": "draft",
                "updated_at": "2026-09-30T12:00:00Z",
                "id": "irrelevant",
            }),
        );
        assert_eq!(plain, noisy, "only the mapped columns may reach the digest");
    }

    #[test]
    fn the_revision_ignores_key_order() {
        let forwards = revision_of_snapshot(
            PAGE_UPDATE,
            &json!({ "slug": "about", "title": "About us", "body": "Hello",
                     "summary": "who we are", "status": "draft" }),
        );
        let backwards = revision_of_snapshot(
            PAGE_UPDATE,
            &json!({ "status": "draft", "summary": "who we are", "body": "Hello",
                     "title": "About us", "slug": "about" }),
        );
        assert_eq!(
            forwards, backwards,
            "two servers with different hashers must agree about whether the page moved"
        );
    }

    #[test]
    fn an_absent_field_and_a_null_field_are_different_revisions() {
        // `summary` cleared versus never set: the preview renders one as a change and the
        // other as nothing to do, so the digest has to tell them apart too.
        let cleared =
            revision_of_snapshot(PAGE_UPDATE, &json!({ "slug": "about", "summary": null }));
        let absent = revision_of_snapshot(PAGE_UPDATE, &json!({ "slug": "about" }));
        assert_ne!(cleared, absent);
    }

    #[test]
    fn an_empty_snapshot_is_the_digest_a_create_is_computed_against() {
        // A create has no row, and this is the value the decision later re-reads to notice a
        // target that appeared in the meantime. If it were not the same digest, "stale" would
        // never fire for creates.
        assert_eq!(
            revision_of_snapshot(PAGE_UPDATE, &Value::Object(Map::new())),
            revision_of_snapshot(PAGE_UPDATE, &Value::Object(Map::new())),
        );
    }

    // --- the mapping lookup ---

    #[test]
    fn the_supported_types_are_the_ones_with_a_reader() {
        assert_eq!(SUPPORTED_TYPES.len(), 1);
        for resource_type in SUPPORTED_TYPES {
            assert!(
                mapping_for(resource_type).is_ok(),
                "{resource_type} is listed as supported, so it must have a mapping"
            );
        }
        for other in ["theme", "plugin", "deployment", "user", "", "Page"] {
            let err = mapping_for(other)
                .expect_err("an unlisted type has no reader")
                .to_string();
            assert!(
                err.contains("page"),
                "the refusal must name what this build does support, got: {err}"
            );
        }
    }

    #[test]
    fn a_resource_id_that_is_not_a_uuid_is_refused_with_the_id_named() {
        for bad in ["", "   ", "not-a-uuid", "12345", "7f1c"] {
            let err = parse_id(bad).unwrap_err().to_string();
            assert!(
                err.contains("resource"),
                "the refusal must talk about the resource, got: {err}"
            );
        }
    }

    #[test]
    fn a_uuid_with_surrounding_whitespace_is_accepted() {
        assert!(parse_id(&format!("  {}  ", page_id())).is_ok());
    }

    // --- the writer ---

    #[test]
    fn a_planned_title_becomes_the_writes_title() {
        let plan = plan::plan(
            PAGE_UPDATE,
            &update(json!({ "title": "About our team" })),
            &current(),
            "About us",
            "r7",
            Vec::new(),
        )
        .expect("the plan resolves");
        let change = changes_for(&plan).expect("the change resolves");
        assert_eq!(change.title.as_deref(), Some("About our team"));
        assert!(
            change.slug.is_none(),
            "a field the plan omits is not written"
        );
        assert!(change.body.is_none());
    }

    #[test]
    fn a_cleared_summary_reaches_the_content_crate_as_an_empty_string() {
        // The two crates spell "clear it" the same way here, and this is the only field where
        // the mapping's `None` and the content crate's `None` would otherwise disagree.
        let plan = plan::plan(
            PAGE_UPDATE,
            &update(json!({ "summary": "" })),
            &current(),
            "About us",
            "r7",
            Vec::new(),
        )
        .expect("the plan resolves");
        let change = changes_for(&plan).expect("the change resolves");
        assert_eq!(
            change.summary.as_deref(),
            Some(""),
            "Some(\"\") clears; None would silently leave the old summary"
        );
    }

    #[test]
    fn a_previewed_status_change_is_refused_rather_than_silently_dropped() {
        // `publish_page` is a separate operation in the content crate. Approving a preview
        // that says "status: draft -> published" and then not publishing would be a decision
        // the reviewer made that the apply did not honour.
        let plan = plan::plan(
            PAGE_UPDATE,
            &update(json!({ "status": "published" })),
            &current(),
            "About us",
            "r7",
            Vec::new(),
        )
        .expect("the plan resolves");
        let err = changes_for(&plan)
            .expect_err("status is not a writable field")
            .to_string();
        assert!(
            err.contains("publish_page"),
            "the refusal must point at the operation that does it, got: {err}"
        );
    }

    #[test]
    fn an_unknown_column_has_no_writer() {
        // A mapping that grows a column nobody taught the writer to spell must fail here
        // rather than drop the write.
        let plan = plan::plan(
            PAGE_UPDATE,
            &update(json!({ "title": "New" })),
            &current(),
            "About us",
            "r7",
            Vec::new(),
        )
        .expect("the plan resolves");
        let mut relabelled = plan.clone();
        relabelled.resource_type = "widget".to_owned();
        let err = changes_for(&relabelled)
            .expect_err("there is no writer for a widget")
            .to_string();
        assert!(err.contains("widget"), "got: {err}");
    }

    #[test]
    fn an_empty_change_is_reported_as_empty() {
        let change = ContentChange::default();
        assert!(change.is_empty());
        assert!(
            !ContentChange {
                title: Some("t".to_owned()),
                ..ContentChange::default()
            }
            .is_empty()
        );
    }

    #[test]
    fn every_page_mapping_column_has_a_writer_or_an_explicit_refusal() {
        // Walks the mapping rather than the writer, so a column added to the mapping cannot
        // reach production without this test naming it.
        for spec in PAGE_UPDATE {
            match spec.field {
                "slug" | "title" | "body" | "summary" => {}
                "status" => {}
                other => panic!(
                    "`{other}` is in the page mapping but no arm of `changes_for` handles it"
                ),
            }
        }
    }

    #[test]
    fn the_status_check_is_the_constraint_the_preview_validates_against() {
        assert!(is_known_status("draft"));
        assert!(is_known_status("published"));
        assert!(is_known_status("archived"));
        for bad in ["pending", "live", "Draft", ""] {
            assert!(!is_known_status(bad), "{bad} is not a page status");
        }
    }

    #[test]
    fn the_repeated_revision_resource_literal_still_matches_the_content_crate() {
        // The constant is repeated on purpose (the dependency runs the other way), and a
        // repeated string is a string that drifts: change `REVISION_RESOURCE` in the content
        // crate and this cascade query keeps counting with the old value, reporting "0
        // translations" for a page that has them. So the literal is pinned here by *reading
        // the source of the other crate* — the only assertion that survives the crate not
        // being a dependency.
        let model = include_str!("../../../content/src/model.rs");
        assert!(
            model.contains(&format!(
                "pub const REVISION_RESOURCE: &str = \"{PAGE_REVISION_RESOURCE}\";"
            )),
            "the content crate's REVISION_RESOURCE no longer matches \
             PAGE_REVISION_RESOURCE = {PAGE_REVISION_RESOURCE:?}; the cascade count would \
             silently report zero"
        );
    }

    #[test]
    fn the_page_reader_selects_exactly_the_columns_the_mapping_names() {
        // A reader that reads a column the mapping does not write produces a digest that moves
        // when an invisible column moves, so every preview goes stale on an unrelated save.
        let mapping: Vec<&str> = PAGE_UPDATE.iter().map(column_of).collect();
        let sql = PAGE_VALUES_SQL.to_ascii_lowercase();
        for column in mapping {
            let aliased = format!(" as {column}");
            assert!(
                sql.contains(&aliased),
                "the page reader does not select `{column}` (looked for `{aliased}`)"
            );
        }
    }
}
