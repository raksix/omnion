//! Block-level revision comparison (REQ-063 §Screens `/pages/<id>/revisions`).
//!
//! A revision compare that shows two JSON payloads side by side answers the question nobody
//! asked. The question is "what did the author change", and the answer is a *list*: this
//! heading was added, that image's alt text was rewritten, this column went away.
//!
//! ## Why the compare is keyed by id, not by position
//!
//! Block ids are client-generated and never rewritten (that is the whole reason
//! `Block::from_value` mints one only when a payload entry has none). Keying the compare on
//! them is what makes a *move* readable: reordering a page is not twenty removals and twenty
//! additions, it is a handful of `moved` rows that say where the block went. A positional diff
//! cannot see a move at all — after a reorder every position differs, so it reports the entire
//! page as rewritten.
//!
//! ## Why the label is a prop, not the whole payload
//!
//! Every row names the block by its *headline* — the first prop in registry order that carries
//! a human string, which is `text` for a heading, `text` for a paragraph, `url` for an image.
//! A diff of forty rows where every row is a JSON object is the thing this replaces.
//!
//! ## Severity
//!
//! `is_fatal` marks a *structural* loss: a block that was there and is not any more, including
//! a subtree that vanished with its parent. Losing a block removes content a visitor was
//! reading; losing a prop is an edit the author made on purpose and can undo by typing.

use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use crate::blocks::Block;

/// How one block changed between two revisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockChange {
    /// The block is in the new revision only.
    Added,
    /// The block is in the old revision only — it was deleted, or its parent was.
    Removed,
    /// Both revisions carry it, and at least one prop or setting changed.
    Changed,
    /// Both carry it with the same content, but it sits somewhere else in the tree.
    Moved,
    /// Both carry it identically, in the same place. Reported only when `include_unchanged`.
    Unchanged,
}

impl BlockChange {
    /// The name the API reports.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Removed => "removed",
            Self::Changed => "changed",
            Self::Moved => "moved",
            Self::Unchanged => "unchanged",
        }
    }

    /// `true` when the change removed content a visitor was reading.
    ///
    /// A deletion is the only row an author has to notice: every other change is either what
    /// they just typed or a position they chose. The revisions screen marks these, and a
    /// restore is offered on them.
    #[must_use]
    pub fn is_fatal(self) -> bool {
        matches!(self, Self::Removed)
    }
}

/// One prop-level difference inside a `changed` block.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PropChange {
    /// Prop name (`text`, `level`, `alt`) or `meta.<setting>` for a per-block setting.
    pub path: String,
    /// Human name the inspector labels the prop with, when the registry declares it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<&'static str>,
    /// Value before the change, rendered as a short string.
    pub before: String,
    /// Value after the change.
    pub after: String,
    /// `true` when the prop is only in the new revision.
    pub added: bool,
    /// `true` when the prop is only in the old revision.
    pub removed: bool,
}

/// One row of the compare.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BlockDiffEntry {
    /// Block id — the same value in both revisions when the block survived.
    pub block_id: Uuid,
    /// Block type, from the new revision for an addition and the old one for a removal.
    pub block_type: String,
    /// What happened.
    pub change: BlockChange,
    /// Headline naming the block in a word or two.
    pub label: String,
    /// Where the block sat before, as `children.0.children.1`.
    pub from_path: String,
    /// Where it sits now, empty for a removal.
    pub to_path: String,
    /// Prop-level detail, for a `changed` block.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub props: Vec<PropChange>,
    /// How many blocks travelled with this one when it was removed (its subtree, itself
    /// included). A deleted `columns` block is one row, not five.
    #[serde(skip_serializing_if = "is_zero")]
    pub removed_count: usize,
}

/// The whole compare.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BlockDiff {
    /// Rows in document order: the new revision's blocks first, then the ones that went away.
    pub entries: Vec<BlockDiffEntry>,
    /// `added + removed + changed + moved`.
    pub added: usize,
    /// Blocks only the old revision has.
    pub removed: usize,
    /// Blocks present in both with a different content.
    pub changed: usize,
    /// Blocks present in both with the same content in a different place.
    pub moved: usize,
    /// `true` when at least one row is fatal, i.e. something was deleted.
    pub has_removals: bool,
}

impl BlockDiff {
    /// A compare that found nothing, for a page that renders from its body.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
            added: 0,
            removed: 0,
            changed: 0,
            moved: 0,
            has_removals: false,
        }
    }

    /// `true` when the two revisions' block trees are the same, in content and in place.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

/// Compare two block trees.
///
/// `before` is the older revision, `after` the newer one; the caller decides which is which,
/// because comparing "left" against "right" is how a person reads it and a restore applies it.
///
/// Rows come out in the new revision's document order followed by the removals, so the list
/// reads top to bottom the way the page does, and a deletion appears where its block used to
/// be rather than exiled to the end.
#[must_use]
pub fn diff_blocks(before: &[Block], after: &[Block]) -> BlockDiff {
    let mut old_index: Vec<(Uuid, &Block, String)> = Vec::new();
    index(before, "", &mut old_index);
    let old_by_id: std::collections::HashMap<Uuid, &Block> = old_index
        .iter()
        .map(|(id, block, _)| (*id, *block))
        .collect();
    let old_path_of = |id: Uuid| {
        old_index
            .iter()
            .find(|(candidate, _, _)| *candidate == id)
            .map(|(_, _, path)| path.clone())
            .unwrap_or_default()
    };

    let mut new_index: Vec<(Uuid, &Block, String)> = Vec::new();
    index(after, "", &mut new_index);

    let mut seen: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
    let mut entries: Vec<BlockDiffEntry> = Vec::new();

    for (id, block, path) in &new_index {
        seen.insert(*id);
        match old_by_id.get(id) {
            Some(old) => {
                let props = prop_changes(old, block);
                let old_path = old_path_of(*id);
                if !props.is_empty() {
                    entries.push(BlockDiffEntry {
                        block_id: *id,
                        block_type: block.kind.clone(),
                        change: BlockChange::Changed,
                        label: headline(block),
                        from_path: old_path,
                        to_path: path.clone(),
                        props,
                        removed_count: 0,
                    });
                } else if old_path != *path {
                    entries.push(BlockDiffEntry {
                        block_id: *id,
                        block_type: block.kind.clone(),
                        change: BlockChange::Moved,
                        label: headline(block),
                        from_path: old_path,
                        to_path: path.clone(),
                        props: Vec::new(),
                        removed_count: 0,
                    });
                }
            }
            None => entries.push(BlockDiffEntry {
                block_id: *id,
                block_type: block.kind.clone(),
                change: BlockChange::Added,
                label: headline(block),
                from_path: String::new(),
                to_path: path.clone(),
                props: Vec::new(),
                removed_count: 0,
            }),
        }
    }

    // The removals, in the old revision's order, so an author reads them against the page they
    // remember. A block whose parent went away is not listed again: `removed_count` on the
    // parent's row already says how many travelled with it, and a duplicate row would read as
    // two separate deletions.
    for (id, block, path) in &old_index {
        if seen.contains(id) || ancestor_was_removed(path, &old_index, &seen) {
            continue;
        }
        entries.push(BlockDiffEntry {
            block_id: *id,
            block_type: block.kind.clone(),
            change: BlockChange::Removed,
            label: headline(block),
            from_path: path.clone(),
            to_path: String::new(),
            props: Vec::new(),
            removed_count: subtree_size(block),
        });
    }

    let added = count(&entries, BlockChange::Added);
    let removed = count(&entries, BlockChange::Removed);
    let changed = count(&entries, BlockChange::Changed);
    let moved = count(&entries, BlockChange::Moved);

    BlockDiff {
        entries,
        added,
        removed,
        changed,
        moved,
        has_removals: removed > 0,
    }
}

/// `true` when some ancestor of `path` did not survive into the new revision.
///
/// A path is `0.children.1`, so the prefixes are `0.children` and `0`; the row list is the only
/// place that knows which id sits at which path, and the walk is short (a tree is capped at
/// three levels), so a scan beats threading a parent pointer through a second pass that only
/// runs for deletions.
fn ancestor_was_removed(
    path: &str,
    old_index: &[(Uuid, &Block, String)],
    survived: &std::collections::HashSet<Uuid>,
) -> bool {
    let mut prefix = path;
    while let Some(cut) = prefix.rfind(".children.") {
        prefix = &prefix[..cut];
        let removed = old_index
            .iter()
            .find(|(_, _, candidate)| candidate == prefix)
            .is_some_and(|(id, _, _)| !survived.contains(id));
        if removed {
            return true;
        }
    }
    false
}

/// Walk a tree into `(id, block, path)` rows, depth first, in document order.
///
/// The lifetime is named rather than elided because the rows borrow the tree they describe, and
/// a nested call has to hand the *same* `&'a` back to its parent — an elided `'_` would be a
/// fresh, shorter lifetime per call and the recursive `out` push would not typecheck.
fn index<'a>(blocks: &'a [Block], prefix: &str, out: &mut Vec<(Uuid, &'a Block, String)>) {
    for (position, block) in blocks.iter().enumerate() {
        let path = if prefix.is_empty() {
            position.to_string()
        } else {
            format!("{prefix}.children.{position}")
        };
        out.push((block.id, block, path.clone()));
        index(&block.children, &path, out);
    }
}

fn count(entries: &[BlockDiffEntry], change: BlockChange) -> usize {
    entries
        .iter()
        .filter(|entry| entry.change == change)
        .count()
}

/// How many blocks a node carries, itself included.
fn subtree_size(block: &Block) -> usize {
    1 + block.children.iter().map(subtree_size).sum::<usize>()
}

/// The prop-level differences between two versions of one block.
///
/// A prop that only exists on one side is reported as an addition or a removal rather than as a
/// change from "absent", because `"before": ""` would claim the author had deleted an empty
/// string — the inspector never writes one, so absence is the honest before.
fn prop_changes(before: &Block, after: &Block) -> Vec<PropChange> {
    let mut out: Vec<PropChange> = Vec::new();
    let before_props = before.props.as_object();
    let after_props = after.props.as_object();
    let definition = crate::blocks::definition(&after.kind);

    let mut keys: Vec<&String> = before_props
        .map(|object| object.keys().collect())
        .unwrap_or_default();
    if let Some(object) = after_props {
        for key in object.keys() {
            if !keys.iter().any(|seen| *seen == key) {
                keys.push(key);
            }
        }
    }
    keys.sort();

    for key in keys {
        let old = before_props.and_then(|object| object.get(key));
        let new = after_props.and_then(|object| object.get(key));
        if old == new {
            continue;
        }
        let label = definition.and_then(|entry| {
            entry
                .props
                .iter()
                .find(|prop| prop.key == key)
                .map(|prop| prop.label)
        });
        out.push(PropChange {
            path: key.clone(),
            label,
            before: render_value(old),
            after: render_value(new),
            added: old.is_none(),
            removed: new.is_none(),
        });
    }

    out.extend(settings_changes("meta", &before.meta, &after.meta));
    out
}

/// Compare the two `meta` objects, flattened so each setting is its own row.
///
/// A setting is not a prop — it has no schema entry and no label — so it is reported under a
/// `meta.` prefix with the registry's own wording for the keys it does name.
fn settings_changes(path_prefix: &str, before: &Value, after: &Value) -> Vec<PropChange> {
    let before_object = before.as_object();
    let after_object = after.as_object();
    if before_object.is_none() && after_object.is_none() {
        return Vec::new();
    }
    let mut keys: Vec<&String> = before_object
        .map(|object| object.keys().collect())
        .unwrap_or_default();
    if let Some(object) = after_object {
        for key in object.keys() {
            if !keys.iter().any(|seen| *seen == key) {
                keys.push(key);
            }
        }
    }
    keys.sort();

    keys.into_iter()
        .filter_map(|key| {
            let old = before_object.and_then(|object| object.get(key));
            let new = after_object.and_then(|object| object.get(key));
            if old == new {
                return None;
            }
            Some(PropChange {
                path: format!("{path_prefix}.{key}"),
                label: None,
                before: render_value(old),
                after: render_value(new),
                added: old.is_none(),
                removed: new.is_none(),
            })
        })
        .collect()
}

/// Render a prop value as something a person can read in a diff row.
///
/// A long body would otherwise make the row unreadable, and a diff is read by comparing two
/// short strings, not by scrolling. 160 characters is roughly two lines in the panel.
fn render_value(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    let rendered = match value {
        Value::String(text) => text.trim().to_owned(),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    if rendered.chars().count() <= MAX_RENDERED {
        return rendered;
    }
    let kept: String = rendered.chars().take(MAX_RENDERED).collect();
    format!("{kept}…")
}

/// Longest a diff cell renders before it is elided.
const MAX_RENDERED: usize = 160;

/// The headline that names a block in a diff row.
///
/// The first prop in **registry order** that carries a human string, so every `text` block
/// leads with its text and every `image` with its alt. Falling back to the type key is right:
/// a brand-new empty block has no text yet, and "Heading" is a better row than `""`.
#[must_use]
pub fn headline(block: &Block) -> String {
    let definition = crate::blocks::definition(&block.kind);
    if let Some(definition) = definition {
        for prop in definition.props {
            if !matches!(
                prop.kind,
                crate::blocks::PropKind::Text | crate::blocks::PropKind::RichText
            ) {
                continue;
            }
            if let Some(text) = block.props.get(prop.key).and_then(Value::as_str) {
                let text = text.trim();
                if !text.is_empty() {
                    return render_value(Some(&Value::String(text.to_owned())));
                }
            }
        }
    }
    definition.map_or_else(|| block.kind.clone(), |entry| entry.label.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn id(seed: &str) -> Uuid {
        Uuid::parse_str(seed).expect("a uuid")
    }

    fn block(kind: &str, props: Value) -> Block {
        Block {
            id: id("9f5b0e0a-2c2f-4a3f-9a3a-0a1b2c3d4e5f"),
            kind: kind.to_owned(),
            props,
            meta: json!({}),
            children: Vec::new(),
        }
    }

    fn with_id(seed: &str, kind: &str, props: Value) -> Block {
        Block {
            id: id(seed),
            ..block(kind, props)
        }
    }

    #[test]
    fn an_identical_tree_produces_no_rows() {
        let before = vec![block("heading", json!({ "text": "Title" }))];
        let after = before.clone();
        assert!(diff_blocks(&before, &after).is_empty());
    }

    #[test]
    fn a_new_block_is_an_addition_named_by_its_text() {
        let after = vec![
            block("heading", json!({ "text": "Title", "level": "h1" })),
            with_id(
                "1b6f0a2c-3d4e-4f50-9a1b-2c3d4e5f6071",
                "text",
                json!({ "text": "A paragraph." }),
            ),
        ];
        let diff = diff_blocks(&after[..1], &after);
        assert_eq!(diff.added, 1);
        assert_eq!(diff.entries[0].change, BlockChange::Added);
        assert_eq!(diff.entries[0].label, "A paragraph.");
        assert!(diff.entries[0].from_path.is_empty());
    }

    #[test]
    fn a_changed_prop_is_named_with_its_inspector_label() {
        let before = vec![block("image", json!({ "url": "/a.png", "alt": "A cat" }))];
        let after = vec![block(
            "image",
            json!({ "url": "/a.png", "alt": "A cat asleep" }),
        )];
        let diff = diff_blocks(&before, &after);
        assert_eq!(diff.changed, 1);
        let prop = &diff.entries[0].props[0];
        assert_eq!(prop.path, "alt");
        assert_eq!(prop.label, Some("Alternative text"));
        assert_eq!(prop.before, "A cat");
        assert_eq!(prop.after, "A cat asleep");
        assert!(!prop.added && !prop.removed);
    }

    #[test]
    fn a_prop_that_only_exists_afterwards_is_an_addition_not_a_change_from_blank() {
        let before = vec![block("image", json!({ "url": "/a.png" }))];
        let after = vec![block("image", json!({ "url": "/a.png", "alt": "A cat" }))];
        let diff = diff_blocks(&before, &after);
        assert!(diff.entries[0].props[0].added);
        assert_eq!(diff.entries[0].props[0].before, "");
    }

    #[test]
    fn a_visibility_change_is_reported_under_meta() {
        let mut before_block = block("text", json!({ "text": "Only wide" }));
        before_block.meta = json!({});
        let mut after_block = before_block.clone();
        after_block.meta = json!({ "hide_on": "mobile" });
        let diff = diff_blocks(&[before_block], &[after_block]);
        assert_eq!(diff.changed, 1);
        assert_eq!(diff.entries[0].props[0].path, "meta.hide_on");
        assert_eq!(diff.entries[0].props[0].after, "mobile");
    }

    #[test]
    fn a_reorder_reads_as_a_move_and_not_as_a_rewrite() {
        let a = with_id(
            "1b6f0a2c-3d4e-4f50-9a1b-2c3d4e5f6071",
            "text",
            json!({ "text": "A" }),
        );
        let b = with_id(
            "2c7e1b3d-4e5f-4061-8b2c-3d4e5f607182",
            "text",
            json!({ "text": "B" }),
        );
        let before = vec![a.clone(), b.clone()];
        let after = vec![b.clone(), a.clone()];
        let diff = diff_blocks(&before, &after);
        assert_eq!(diff.moved, 2);
        assert_eq!(diff.added, 0, "a move is not an addition");
        assert_eq!(diff.removed, 0, "a move is not a removal");

        // Rows come out in the NEW document order, so the block that is now first is row 0 —
        // and it is the one that was second. Indexing by position to work out *which* row is
        // which is exactly the mistake a positional diff makes; looking the row up by id is
        // what the id is for.
        let row = |wanted: Uuid| {
            diff.entries
                .iter()
                .find(|entry| entry.block_id == wanted)
                .unwrap_or_else(|| panic!("a row for {wanted}"))
                .clone()
        };
        // `after` is `[b, a]`, so the block that was first is now second.
        assert_eq!(row(a.id).to_path, "1");
        assert_eq!(row(a.id).from_path, "0");
        assert_eq!(row(b.id).to_path, "0");
        assert_eq!(row(b.id).from_path, "1");
    }

    #[test]
    fn a_deleted_block_is_fatal_and_counts_its_subtree() {
        let mut columns = with_id(
            "3d8f2c4e-5f60-4172-9c3d-4e5f60718293",
            "columns",
            json!({ "columns": 2 }),
        );
        columns.children = vec![
            with_id(
                "4e9a3d5f-6071-4283-8d4e-5f60718293a4",
                "text",
                json!({ "text": "A" }),
            ),
            with_id(
                "5fab4e60-7182-4394-9e5f-60718293a4b5",
                "text",
                json!({ "text": "B" }),
            ),
        ];
        let diff = diff_blocks(&[columns], &[]);
        assert_eq!(diff.removed, 1, "the children travel with their parent");
        assert_eq!(diff.entries[0].removed_count, 3);
        assert!(diff.has_removals);
        assert!(diff.entries[0].change.is_fatal());
    }

    #[test]
    fn a_child_that_survives_its_parent_is_not_reported_twice() {
        let mut columns = with_id(
            "3d8f2c4e-5f60-4172-9c3d-4e5f60718293",
            "columns",
            json!({ "columns": 2 }),
        );
        let child = with_id(
            "4e9a3d5f-6071-4283-8d4e-5f60718293a4",
            "text",
            json!({ "text": "A" }),
        );
        columns.children = vec![child.clone()];
        // The container is gone and the child was lifted out to the top level. The child did
        // not appear — it MOVED, which is the honest reading: its id is in both revisions, so
        // claiming it was added would tell the author they gained a paragraph they already had.
        let diff = diff_blocks(&[columns], &[child]);
        assert_eq!(diff.moved, 1, "{diff:?}");
        assert_eq!(diff.added, 0, "a lifted child was not created");
        assert_eq!(diff.removed, 1, "its container was");
        assert_eq!(
            diff.entries.iter().filter(|e| e.change.is_fatal()).count(),
            1,
            "one deletion, not two"
        );
        let moved = &diff.entries[0];
        assert_eq!(moved.block_id, id("4e9a3d5f-6071-4283-8d4e-5f60718293a4"));
        assert_eq!(moved.from_path, "0.children.0");
        assert_eq!(moved.to_path, "0");
    }

    #[test]
    fn a_nested_move_reports_both_its_old_and_its_new_path() {
        let mut columns_before = with_id(
            "3d8f2c4e-5f60-4172-9c3d-4e5f60718293",
            "columns",
            json!({ "columns": 2 }),
        );
        let child = with_id(
            "4e9a3d5f-6071-4283-8d4e-5f60718293a4",
            "text",
            json!({ "text": "A" }),
        );
        columns_before.children = vec![child.clone()];
        let mut columns_after = columns_before.clone();
        columns_after.children = vec![with_id(
            "6a0c5f71-8293-44a5-8f60-718293a4b5c6",
            "text",
            json!({ "text": "B" }),
        )];
        columns_after.children.push(child);
        let diff = diff_blocks(&[columns_before], &[columns_after]);
        assert_eq!(diff.moved, 1);
        // Two rows at least: the sibling that arrived is an addition and comes first, because
        // rows follow the new document order.
        let moved = diff
            .entries
            .iter()
            .find(|entry| entry.change == BlockChange::Moved)
            .expect("the moved row");
        assert_eq!(moved.from_path, "0.children.0");
        assert_eq!(moved.to_path, "0.children.1");
        assert_eq!(diff.added, 1, "the new sibling is an addition");
        assert_eq!(diff.removed, 0);
    }

    #[test]
    fn a_long_value_is_elided_rather_than_filling_the_row() {
        let long = "x".repeat(MAX_RENDERED + 40);
        let mut after = block("text", json!({ "text": long.clone() }));
        after.props = json!({ "text": long });
        let diff = diff_blocks(&[block("text", json!({ "text": "short" }))], &[after]);
        let rendered = &diff.entries[0].props[0].after;
        assert_eq!(
            rendered.chars().count(),
            MAX_RENDERED + 1,
            "elided with an ellipsis"
        );
        assert!(rendered.ends_with('…'));
    }

    #[test]
    fn a_page_that_renders_from_its_body_compares_as_two_empty_trees() {
        assert!(diff_blocks(&[], &[]).is_empty());
        assert_eq!(diff_blocks(&[], &[]).removed, 0);
    }
}
