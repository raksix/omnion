//! Content patterns and page templates (REQ-063, slice 3).
//!
//! A **pattern** is a named, reusable group of blocks. A **page template** is a whole page's
//! worth of blocks with sample content. Both are stored as the same JSON a revision carries, and
//! that shared shape is the decision everything else here follows from: inserting a pattern
//! copies its blocks and mints fresh ids, so the tree the author then edits *is* the tree the
//! pattern described, and a diff of the page afterwards speaks about the same blocks. A second
//! representation would make "insert" a conversion, and a conversion is where content quietly
//! loses a prop.
//!
//! Three rules the store keeps:
//!
//! - both are organization-scoped, because a pattern is reused across an organization's sites;
//! - a key is unique per organization, so `POST` twice with one key is an update, not a second
//!   row the author has to choose between;
//! - the platform's own templates are `is_system` rows seeded from code, and a system template
//!   cannot be deleted — a *built from* page is not the template.

use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::blocks::{Block, blocks_to_value, parse_blocks, sanitize_tree};
use crate::error::{ContentError, Result};
use crate::model::{DEFAULT_PAGE_TYPE, NewPage, Page, PageChanges, PageRevision};
use crate::pages;
use crate::validation::{validate_key, validate_optional_text, validate_page_type, validate_text};

/// Longest accepted pattern/template name.
pub const MAX_NAME_LENGTH: usize = 120;

/// Longest accepted description.
pub const MAX_DESCRIPTION_LENGTH: usize = 500;

/// Column list for every `Pattern` query.
const PATTERN_COLUMNS: &str = "id, organization_id, key, name, category, description, blocks, created_by, created_at, updated_at";

/// Column list for every `PageTemplate` query.
const TEMPLATE_COLUMNS: &str = "id, organization_id, key, name, page_type, description, blocks, \
     is_system, created_by, created_at, updated_at";

/// A reusable block group.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Pattern {
    /// Primary key.
    pub id: Uuid,
    /// Organization the pattern belongs to.
    pub organization_id: Uuid,
    /// Stable key, unique inside the organization (`hero-cta`).
    pub key: String,
    /// Name the library card shows.
    pub name: String,
    /// Grouping the library filters by.
    pub category: String,
    /// One line describing the group, when the author wrote one.
    pub description: Option<String>,
    /// The block group, as stored JSON.
    pub blocks: Value,
    /// Account that created the pattern, when a person did.
    pub created_by: Option<Uuid>,
    /// Creation timestamp.
    pub created_at: time::OffsetDateTime,
    /// Last change.
    pub updated_at: time::OffsetDateTime,
}

impl Pattern {
    /// Number of blocks in the group, nested ones included — the library card's own count.
    ///
    /// Computed from the stored JSON rather than a column, so a pattern edited through any
    /// writer reports the truth instead of a counter that drifts.
    #[must_use]
    pub fn block_count(&self) -> usize {
        parse_blocks(&self.blocks).map_or(0, |blocks| {
            blocks.iter().map(count_with_children).sum::<usize>()
        })
    }
}

/// A whole-page block template with sample content.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct PageTemplate {
    /// Primary key.
    pub id: Uuid,
    /// Organization the template belongs to.
    pub organization_id: Uuid,
    /// Stable key, unique inside the organization (`landing`).
    pub key: String,
    /// Name the gallery card shows.
    pub name: String,
    /// Content type a page created from this template gets.
    pub page_type: String,
    /// One line describing the template, when there is one.
    pub description: Option<String>,
    /// The page's blocks, as stored JSON.
    pub blocks: Value,
    /// `true` for the templates that ship with the platform.
    pub is_system: bool,
    /// Account that created the template, when a person did.
    pub created_by: Option<Uuid>,
    /// Creation timestamp.
    pub created_at: time::OffsetDateTime,
    /// Last change.
    pub updated_at: time::OffsetDateTime,
}

impl PageTemplate {
    /// Blocks in the template, nested included.
    #[must_use]
    pub fn block_count(&self) -> usize {
        parse_blocks(&self.blocks).map_or(0, |blocks| {
            blocks.iter().map(count_with_children).sum::<usize>()
        })
    }
}

/// A pattern to save: create when the key is new, update when it already exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPattern {
    /// Organization the pattern belongs to.
    pub organization_id: Uuid,
    /// Stable key; normalised to lowercase before the write.
    pub key: String,
    /// Name the library card shows.
    pub name: String,
    /// Grouping; `general` when omitted.
    pub category: Option<String>,
    /// One line describing the group.
    pub description: Option<String>,
    /// The block group.
    pub blocks: Value,
    /// Author, when a person did it.
    pub created_by: Option<Uuid>,
}

/// A pattern's changed fields. `None` leaves a field untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PatternChanges {
    /// New name.
    pub name: Option<String>,
    /// New category.
    pub category: Option<String>,
    /// New description; an empty string clears it.
    pub description: Option<String>,
    /// New block group.
    pub blocks: Option<Value>,
}

impl PatternChanges {
    /// `true` when the change set carries nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.category.is_none()
            && self.description.is_none()
            && self.blocks.is_none()
    }
}

/// A template to create or replace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTemplate {
    /// Organization the template belongs to.
    pub organization_id: Uuid,
    /// Stable key.
    pub key: String,
    /// Name the gallery card shows.
    pub name: String,
    /// Content type a page created from it gets; `page` when omitted.
    pub page_type: Option<String>,
    /// One line describing it.
    pub description: Option<String>,
    /// The page's blocks.
    pub blocks: Value,
    /// `true` for the platform's own templates.
    pub is_system: bool,
    /// Author, when a person did it.
    pub created_by: Option<Uuid>,
}

/// A page to create from a template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageFromTemplate {
    /// Site the page belongs to.
    pub site_id: Uuid,
    /// Template whose blocks seed the page.
    pub template_id: Uuid,
    /// Organization the template must belong to — the site's own.
    pub organization_id: Uuid,
    /// Address of the new page.
    pub slug: String,
    /// Title of the new page.
    pub title: String,
    /// Author, when a person did it.
    pub created_by: Option<Uuid>,
}

// ---------------------------------------------------------------------------------------------
// Patterns
// ---------------------------------------------------------------------------------------------

/// List an organization's patterns, newest first, optionally narrowed to one category.
pub async fn list_patterns(
    pool: &PgPool,
    organization_id: Uuid,
    category: Option<&str>,
) -> Result<Vec<Pattern>> {
    let category = match category {
        Some(category) => Some(validate_key(category, "category")?),
        None => None,
    };
    let sql = format!(
        "select {PATTERN_COLUMNS} from content_patterns \
         where organization_id = $1 and ($2::text is null or category = $2) \
         order by created_at desc, id asc"
    );
    sqlx::query_as::<_, Pattern>(&sql)
        .bind(organization_id)
        .bind(category)
        .fetch_all(pool)
        .await
        .map_err(Into::into)
}

/// Read one pattern of an organization.
pub async fn find_pattern(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
) -> Result<Option<Pattern>> {
    let sql = format!(
        "select {PATTERN_COLUMNS} from content_patterns \
         where id = $1 and organization_id = $2"
    );
    sqlx::query_as::<_, Pattern>(&sql)
        .bind(id)
        .bind(organization_id)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

/// Read one pattern by key — the "insert" path names a key, not an id.
pub async fn find_pattern_by_key(
    pool: &PgPool,
    organization_id: Uuid,
    key: &str,
) -> Result<Option<Pattern>> {
    let key = validate_key(key, "key")?;
    let sql = format!(
        "select {PATTERN_COLUMNS} from content_patterns \
         where organization_id = $1 and key = $2"
    );
    sqlx::query_as::<_, Pattern>(&sql)
        .bind(organization_id)
        .bind(&key)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

/// Save a pattern, replacing an existing one with the same key.
///
/// The key is the identity an author remembers, and the store enforces it unique per
/// organization — so "save my changes" is a `POST` to the same endpoint rather than a second
/// route that does the same thing with a different error when the author forgot.
pub async fn save_pattern(pool: &PgPool, new: NewPattern) -> Result<Pattern> {
    let key = validate_key(&new.key, "key")?;
    let name = validate_text(&new.name, MAX_NAME_LENGTH, "name")?;
    let category = match new.category.as_deref() {
        Some(category) => validate_key(category, "category")?,
        None => "general".to_owned(),
    };
    let description = validate_optional_text(new.description.as_deref(), MAX_DESCRIPTION_LENGTH)?;
    let blocks = normalize_block_payload(new.blocks)?;

    let sql = format!(
        "insert into content_patterns \
         (organization_id, key, name, category, description, blocks, created_by) \
         values ($1, $2, $3, $4, $5, $6::jsonb, $7) \
         on conflict (organization_id, key) do update set \
           name = excluded.name, category = excluded.category, \
           description = excluded.description, blocks = excluded.blocks, \
           updated_at = now() \
         returning {PATTERN_COLUMNS}"
    );
    sqlx::query_as::<_, Pattern>(&sql)
        .bind(new.organization_id)
        .bind(&key)
        .bind(&name)
        .bind(&category)
        .bind(description.as_deref())
        .bind(&blocks)
        .bind(new.created_by)
        .fetch_one(pool)
        .await
        .map_err(|error| map_pattern_write_error(error, &key))
}

/// Apply `changes` to a pattern. An empty change set is a no-op, not a touch of `updated_at`:
/// the library's "last edited" is then an answer rather than a heartbeat.
pub async fn update_pattern(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
    changes: &PatternChanges,
) -> Result<Pattern> {
    let name = match changes.name.as_deref() {
        Some(name) => Some(validate_text(name, MAX_NAME_LENGTH, "name")?),
        None => None,
    };
    let category = match changes.category.as_deref() {
        Some(category) => Some(validate_key(category, "category")?),
        None => None,
    };
    let description = match changes.description.as_deref() {
        Some(description) => validate_optional_text(Some(description), MAX_DESCRIPTION_LENGTH)?,
        None => None,
    };
    let blocks = match changes.blocks.clone() {
        Some(blocks) => Some(normalize_block_payload(blocks)?),
        None => None,
    };

    if changes.is_empty() {
        return find_pattern(pool, organization_id, id)
            .await?
            .ok_or(ContentError::PatternNotFound);
    }

    let sql = format!(
        "update content_patterns set \
           name = coalesce($3, name), \
           category = coalesce($4, category), \
           description = case when $5::boolean then $6 else description end, \
           blocks = coalesce($7::jsonb, blocks), \
           updated_at = now() \
         where id = $1 and organization_id = $2 \
         returning {PATTERN_COLUMNS}"
    );
    sqlx::query_as::<_, Pattern>(&sql)
        .bind(id)
        .bind(organization_id)
        .bind(name)
        .bind(category)
        .bind(changes.description.is_some())
        .bind(description)
        .bind(blocks)
        .fetch_optional(pool)
        .await
        .map_err(ContentError::from)?
        .ok_or(ContentError::PatternNotFound)
}

/// Remove a pattern. `false` when the organization has no such pattern.
pub async fn delete_pattern(pool: &PgPool, organization_id: Uuid, id: Uuid) -> Result<bool> {
    let deleted =
        sqlx::query("delete from content_patterns where id = $1 and organization_id = $2")
            .bind(id)
            .bind(organization_id)
            .execute(pool)
            .await?;
    Ok(deleted.rows_affected() > 0)
}

/// Copy a pattern's blocks into a page's block list, with fresh ids.
///
/// This is the whole of "insert a pattern": take the stored tree, give every block a new id, and
/// hand it back. Fresh ids are the whole point — a pattern inserted into three pages must not
/// leave three blocks sharing one id, or the inspector's selection and the revision diff both
/// become ambiguous about which of them an author meant. The block *types* and *props* travel
/// untouched, which is what makes the criterion "reproduces the block tree exactly" checkable:
/// the ids differ and nothing else does.
pub fn instance_blocks(stored: &Value) -> Result<Vec<Block>> {
    let mut blocks = parse_blocks(stored)?;
    reidentify(&mut blocks);
    Ok(blocks)
}

fn reidentify(blocks: &mut [Block]) {
    for block in blocks {
        block.id = Uuid::new_v4();
        reidentify(&mut block.children);
    }
}

// ---------------------------------------------------------------------------------------------
// Page templates
// ---------------------------------------------------------------------------------------------

/// List an organization's templates, name order.
pub async fn list_templates(pool: &PgPool, organization_id: Uuid) -> Result<Vec<PageTemplate>> {
    let sql = format!(
        "select {TEMPLATE_COLUMNS} from content_page_templates \
         where organization_id = $1 order by name asc, key asc"
    );
    sqlx::query_as::<_, PageTemplate>(&sql)
        .bind(organization_id)
        .fetch_all(pool)
        .await
        .map_err(Into::into)
}

/// Read one template of an organization.
pub async fn find_template(
    pool: &PgPool,
    organization_id: Uuid,
    id: Uuid,
) -> Result<Option<PageTemplate>> {
    let sql = format!(
        "select {TEMPLATE_COLUMNS} from content_page_templates \
         where id = $1 and organization_id = $2"
    );
    sqlx::query_as::<_, PageTemplate>(&sql)
        .bind(id)
        .bind(organization_id)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

/// Read one template by key.
pub async fn find_template_by_key(
    pool: &PgPool,
    organization_id: Uuid,
    key: &str,
) -> Result<Option<PageTemplate>> {
    let key = validate_key(key, "key")?;
    let sql = format!(
        "select {TEMPLATE_COLUMNS} from content_page_templates \
         where organization_id = $1 and key = $2"
    );
    sqlx::query_as::<_, PageTemplate>(&sql)
        .bind(organization_id)
        .bind(&key)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

/// Save a template, replacing an existing one with the same key.
pub async fn save_template(pool: &PgPool, new: NewTemplate) -> Result<PageTemplate> {
    let key = validate_key(&new.key, "key")?;
    let name = validate_text(&new.name, MAX_NAME_LENGTH, "name")?;
    let page_type = match new.page_type.as_deref() {
        Some(page_type) => validate_page_type(page_type)?,
        None => DEFAULT_PAGE_TYPE.to_owned(),
    };
    let description = validate_optional_text(new.description.as_deref(), MAX_DESCRIPTION_LENGTH)?;
    let blocks = normalize_block_payload(new.blocks)?;

    let sql = format!(
        "insert into content_page_templates \
         (organization_id, key, name, page_type, description, blocks, is_system, created_by) \
         values ($1, $2, $3, $4, $5, $6::jsonb, $7, $8) \
         on conflict (organization_id, key) do update set \
           name = excluded.name, page_type = excluded.page_type, \
           description = excluded.description, blocks = excluded.blocks, \
           is_system = excluded.is_system \
         returning {TEMPLATE_COLUMNS}"
    );
    sqlx::query_as::<_, PageTemplate>(&sql)
        .bind(new.organization_id)
        .bind(&key)
        .bind(&name)
        .bind(&page_type)
        .bind(description.as_deref())
        .bind(&blocks)
        .bind(new.is_system)
        .bind(new.created_by)
        .fetch_one(pool)
        .await
        .map_err(|error| map_template_write_error(error, &key))
}

/// Create a page from a template: a real page plus its first draft revision, blocks and all.
///
/// The template is read *inside* the same transaction that writes the page, so a template deleted
/// between the gallery and the form answers "no such template" rather than creating a page with
/// no blocks — and the page is a genuine page, not a link to the template: editing it does not
/// change the template, and the template can be deleted afterwards.
pub async fn create_page_from_template(
    pool: &PgPool,
    new: &PageFromTemplate,
) -> Result<(Page, PageRevision)> {
    let template = find_template(pool, new.organization_id, new.template_id)
        .await?
        .ok_or(ContentError::TemplateNotFound)?;

    let mut blocks = instance_blocks(&template.blocks)?;
    // A template is authored content that lands on a public page, so it goes through the same
    // sanitiser a page's own save does rather than arriving pre-trusted.
    sanitize_tree(&mut blocks);

    let (page, _) = pages::create_page(
        pool,
        NewPage {
            site_id: new.site_id,
            slug: new.slug.clone(),
            page_type: Some(template.page_type.clone()),
            title: new.title.clone(),
            body: Some(block_text(&blocks)),
            summary: template.description.clone(),
            created_by: new.created_by,
        },
    )
    .await?;

    // `create_page` writes revision 1 with an empty tree. The template's blocks land as the
    // next draft revision rather than by editing revision 1 in place, because a revision's
    // content is never rewritten (docs/05-VERSIONING.md §4) — the same rule a manual save obeys.
    // The page therefore arrives at the author as revision 2, which is honest: it was built
    // from something and then filled in.
    let changes = PageChanges {
        blocks: Some(blocks_to_value(&blocks)),
        ..PageChanges::default()
    };
    let page = pages::update_page(pool, page.id, &changes, new.created_by).await?;
    let draft = pages::current_draft(pool, page.id)
        .await?
        .ok_or(ContentError::NoDraftRevision)?;
    Ok((page, draft))
}

/// Delete a template.
///
/// A system template is refused: the gallery offers the platform's own starting points, and
/// removing one would leave a "New page from template" flow that cannot deliver what it
/// advertises. A custom template is the author's to delete.
pub async fn delete_template(pool: &PgPool, organization_id: Uuid, id: Uuid) -> Result<bool> {
    let sql = "delete from content_page_templates \
               where id = $1 and organization_id = $2 and is_system = false";
    let deleted = sqlx::query(sql)
        .bind(id)
        .bind(organization_id)
        .execute(pool)
        .await?;
    Ok(deleted.rows_affected() > 0)
}

// ---------------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------------

/// Parse, sanitise and re-check a block payload on its way into storage.
///
/// Sanitising here is not a second opinion: a pattern is inserted into pages, so a `raw_html`
/// block saved through the library would be a way to store markup that never went through the
/// page save's own filter. The store normalises, sanitises, and *then* validates, so a pattern
/// that is refused names the same issue codes the editor already shows.
fn normalize_block_payload(value: Value) -> Result<Value> {
    let mut blocks = parse_blocks(&value)?;
    sanitize_tree(&mut blocks);
    let report = crate::blocks::validate(&blocks_to_value(&blocks));
    if let Some(issue) = report.issues.iter().find(|issue| issue.is_fatal()) {
        return Err(ContentError::InvalidBlock(format!(
            "{} ({})",
            issue.message, issue.code
        )));
    }
    // The registry's defaults are filled in here for the same reason `pages::update_page` fills
    // them in: a stored payload is one the renderer can draw without guessing. A pattern kept
    // its props sparse while the page it was cut from stored them whole, so "insert this
    // pattern" would have landed a page whose tree the revision diff then reported as *every
    // block changed* — the author had changed nothing, the two writers had normalised
    // differently. One normaliser, one stored shape.
    for block in &mut blocks {
        // `normalize` is `#[must_use]` (it returns the block) but mutates in place —
        // a `let _ =` here would be the only way to satisfy the attribute without
        // looking like a discarded value.
        let _normalized = crate::blocks::normalize(block);
    }
    Ok(blocks_to_value(&blocks))
}

/// `true` when a stored pattern would block a page it is inserted into.
///
/// Separate from [`normalize_block_payload`]'s refusal because the two answer different
/// questions, and the page save is where the distinction is already drawn (see
/// [`crate::blocks::BlockIssue::is_fatal`]). A *fatal* issue is a payload the store cannot
/// hold at all, so saving refuses it. An unfinished or misplaced block — a heading with no
/// text, a column outside a columns block — stores fine and blocks the *publish* instead,
/// which is what lets an author be mid-sentence. A pattern library needs the same split: a
/// half-built pattern is a legitimate thing to save and finish later, and refusing it would
/// mean an author could not cut a pattern out of a page that is itself half-built.
#[must_use]
pub fn pattern_needs_attention(stored: &Value) -> Vec<crate::blocks::BlockIssue> {
    parse_blocks(stored)
        .map(|blocks| {
            crate::blocks::validate(&blocks_to_value(&blocks))
                .issues
                .into_iter()
                .filter(|issue| issue.is_error())
                .collect()
        })
        .unwrap_or_default()
}

/// Count a block and everything under it.
fn count_with_children(block: &Block) -> usize {
    1 + block
        .children
        .iter()
        .map(count_with_children)
        .sum::<usize>()
}

/// The text a page created from a template carries in its `body`.
///
/// A revision renders from its `blocks` when it has them, but the body is what search, the SEO
/// fields and any export read — and a page created from a template with an empty body would be
/// findable by nothing. The text is the blocks' own copy, so the two never disagree.
fn block_text(blocks: &[Block]) -> String {
    let mut out: Vec<String> = Vec::new();
    for block in blocks {
        if let Some(object) = block.props.as_object() {
            for value in object.values() {
                if let Some(text) = value.as_str() {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        out.push(trimmed.to_owned());
                    }
                }
            }
        }
        out.extend(block_text(&block.children).split('\n').map(str::to_owned));
    }
    out.retain(|line| !line.is_empty());
    out.join("\n")
}

fn map_pattern_write_error(error: sqlx::Error, key: &str) -> ContentError {
    if let sqlx::Error::Database(db) = &error {
        if db.code().as_deref() == Some("23505") {
            return ContentError::PatternKeyTaken(key.to_owned());
        }
    }
    ContentError::Database(error)
}

fn map_template_write_error(error: sqlx::Error, key: &str) -> ContentError {
    if let sqlx::Error::Database(db) = &error {
        if db.code().as_deref() == Some("23505") {
            return ContentError::TemplateKeyTaken(key.to_owned());
        }
    }
    ContentError::Database(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tree() -> Value {
        json!([
            {
                "id": "11111111-1111-1111-1111-111111111111",
                "type": "heading",
                "props": { "text": "Welcome", "level": "h1" }
            },
            {
                "id": "22222222-2222-2222-2222-222222222222",
                "type": "columns",
                "props": { "columns": 2 },
                "children": [
                    { "id": "33333333-3333-3333-3333-333333333333", "type": "column",
                      "props": {}, "children": [] },
                    { "id": "44444444-4444-4444-4444-444444444444", "type": "column",
                      "props": {}, "children": [
                        { "id": "55555555-5555-5555-5555-555555555555", "type": "text",
                          "props": { "text": "A column with words." } }
                    ] }
                ]
            }
        ])
    }

    #[test]
    fn inserting_a_pattern_copies_the_tree_with_fresh_ids() {
        let original = parse_blocks(&tree()).expect("a readable tree");
        let inserted = instance_blocks(&tree()).expect("a readable tree");

        // Same shape: the same types in the same nesting, the same props. The comparison
        // strips ids rather than re-minting them, because a second `reidentify` would compare
        // two *different* random sets and the assertion could only ever pass by accident.
        assert_eq!(
            strip_ids(&blocks_to_value(&original)),
            strip_ids(&blocks_to_value(&inserted)),
            "an inserted pattern differs from its source in ids and nothing else",
        );

        let before: Vec<Uuid> = walk_ids(&original);
        let after: Vec<Uuid> = walk_ids(&inserted);
        assert_eq!(
            before.len(),
            after.len(),
            "the block count travels with the pattern"
        );
        for (was, now) in before.iter().zip(after.iter()) {
            assert_ne!(
                was, now,
                "a block inserted from a pattern must not keep the source's id"
            );
        }
    }

    #[test]
    fn two_insertions_of_one_pattern_never_collide() {
        let first = instance_blocks(&tree()).expect("a readable tree");
        let second = instance_blocks(&tree()).expect("a readable tree");
        let first_ids: Vec<Uuid> = walk_ids(&first);
        let second_ids: Vec<Uuid> = walk_ids(&second);
        for id in &first_ids {
            assert!(
                !second_ids.contains(id),
                "the same pattern inserted twice must not share a block id"
            );
        }
    }

    #[test]
    fn a_pattern_the_store_cannot_hold_at_all_is_refused() {
        // An unknown type is fatal — not "unfinished", but unreadable — so the library refuses
        // it exactly as a page save would. A pattern the platform cannot render is a dead row.
        let unknown = json!([
            { "id": "11111111-1111-1111-1111-111111111111", "type": "carousel", "props": {} }
        ]);
        let error = normalize_block_payload(unknown).expect_err("an unknown type is fatal");
        assert!(
            matches!(error, ContentError::InvalidBlock(_)),
            "got {error:?}"
        );
    }

    #[test]
    fn a_half_built_pattern_stores_and_reports_what_it_still_needs() {
        // An orphan column blocks the *publish*, not the save — the same split the page save
        // makes, so an author can cut a pattern out of a page that is itself half-built.
        let orphan = json!([
            { "id": "11111111-1111-1111-1111-111111111111", "type": "column", "props": {} }
        ]);
        let stored = normalize_block_payload(orphan).expect("an unfinished pattern still saves");

        let issues = pattern_needs_attention(&stored);
        assert!(
            issues
                .iter()
                .any(|issue| issue.code == "block_column_orphan"),
            "the library card says what is still wrong: {issues:?}"
        );
    }

    #[test]
    fn a_finished_pattern_reports_nothing_to_attend_to() {
        let stored = normalize_block_payload(tree()).expect("a complete pattern saves");
        let issues = pattern_needs_attention(&stored);
        assert!(
            issues.is_empty(),
            "a complete pattern must not nag its author: {:?}",
            issues.iter().map(|i| i.code).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_pattern_is_stored_sanitised() {
        let dirty = json!([
            { "id": "11111111-1111-1111-1111-111111111111", "type": "raw_html",
              "props": { "html": "<p>ok</p><script>alert(1)</script>" } }
        ]);
        let stored = normalize_block_payload(dirty).expect("a saveable tree");
        let text = stored.to_string();
        assert!(
            !text.contains("script"),
            "the stored payload keeps no script: {text}"
        );
        assert!(text.contains("ok"), "the safe part of the markup is kept");
    }

    #[test]
    fn block_text_is_what_search_and_seo_read() {
        let blocks = instance_blocks(&tree()).expect("a readable tree");
        let text = block_text(&blocks);
        assert!(
            text.contains("Welcome"),
            "a heading's words are in the body: {text}"
        );
        assert!(
            text.contains("A column with words."),
            "a nested block's words are in the body too: {text}"
        );
    }

    /// A block payload with every `id` removed, for comparing two trees' shape.
    fn strip_ids(value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Array(items) => {
                serde_json::Value::Array(items.iter().map(strip_ids).collect())
            }
            serde_json::Value::Object(object) => serde_json::Value::Object(
                object
                    .iter()
                    .filter(|(key, _)| key.as_str() != "id")
                    .map(|(key, value)| (key.clone(), strip_ids(value)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    fn walk_ids(blocks: &[Block]) -> Vec<Uuid> {
        blocks
            .iter()
            .flat_map(|block| {
                let mut ids = vec![block.id];
                ids.extend(walk_ids(&block.children));
                ids
            })
            .collect()
    }
}

#[test]
fn probe_which_prop() {
    let payload = serde_json::json!([
        { "id": "11111111-1111-1111-1111-111111111111", "type": "heading",
          "props": { "text": "Welcome", "level": "h1" } },
        { "id": "22222222-2222-2222-2222-222222222222", "type": "columns",
          "props": { "columns": 2 },
          "children": [
            { "id": "33333333-3333-3333-3333-333333333333", "type": "column", "props": {}, "children": [] },
            { "id": "44444444-4444-4444-4444-444444444444", "type": "column", "props": {},
              "children": [ { "id": "55555555-5555-5555-5555-555555555555", "type": "text",
                              "props": { "text": "A column with words." } } ] }
          ] }
    ]);
    let r = crate::validate(&payload);
    for i in &r.issues {
        println!(
            "PROBE {} {} {} sev={}",
            i.block_id, i.path, i.code, i.severity
        );
    }
}
