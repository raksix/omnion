//! Omnion content · the media a block tree names, and what happens when it is gone (REQ-063).
//!
//! A block stores a **media id** in its props — `image.src`, every entry of `gallery.images` — and
//! the renderer turns that id into `/api/v1/public/media/{id}`. That indirection is the whole
//! point: the file is one row, the page is another, and an author's picture can be replaced
//! without touching a single page. It is also a promise, and this module is what keeps it: the
//! promise is that a page names files which **exist and can be served**, and a file somebody
//! trashes in the media library breaks it silently.
//!
//! ## Why this is not an event consumer
//!
//! `media.deleted` is a real event (REQ-010 emits it) and a consumer is the obvious place to react.
//! It is the wrong place, for the same reason `featured.rs` computes its own warning rather than
//! listening: a consumer that has not run — because it was offline, because the file was trashed
//! before the consumer existed, because the backoff ladder gave up — leaves a published page
//! serving a dead image URL with nothing on screen saying why. The degradation is a **property of
//! the join**, so it is derived from the join, on read, every time.
//!
//! ## The three answers, and why a trashed file is not a missing one
//!
//! | the id names | what the renderer draws | what the panel says |
//! |---|---|---|
//! | a live file | the image | nothing |
//! | a **trashed** file | nothing (an `image` block degrades to its caption) | "this image is in the trash — restore it, or pick another one" |
//! | **nothing at all** (purged) | nothing | "this image was deleted — pick another one" |
//!
//! A hand-written URL is a fourth case that is *not* on the list because it is not ours: the
//! platform did not upload it, does not own it, and cannot know whether it answers 404 tomorrow.
//! Warning about it would teach authors that the panel nags about every external image.
//!
//! The trashed/purged split is the same one `featured.rs` draws, and for the same reason: they
//! need different words. "Restore it" is actionable for a trash and meaningless for a purge, so an
//! answer that collapsed them would send an author to empty the trash for a file that is not in
//! it.
//!
//! **It is a warning, never an error.** A page that lost a picture is still a page, and refusing
//! the publish would take a working page off the site because a file was deleted on purpose — the
//! exact failure `featured.rs` documents for the one-image case. A warning that never blocks is
//! also what makes it safe to report on *every* read, including the public one, where there is no
//! author to act on it: the renderer degrades and the operator sees the note.
//!
//! ## One query, not one query per block
//!
//! The ids are collected in one walk and resolved in a single `select … where id = any($1)`,
//! then written back into the tree. A page with forty gallery images therefore costs one round
//! trip, and — the part that actually matters — the answer is **the same for every reader of the
//! same page**: a tree resolved block-by-block would make "is this image gone?" a function of how
//! many blocks came before it, and the warnings would move around between two loads of one page.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::blocks::{Block, Viewport, block_hide_on};
use crate::error::{ContentError, Result};

// ---------------------------------------------------------------------------------------------
// Bounds and the media-addressing props
// ---------------------------------------------------------------------------------------------

/// Longest `?media=` filter the route accepts.
///
/// The preview frame's `GET /pages/{id}/preview?media=<id>` is how the editor asks "would this
/// block look right if that file were gone?" without an operator trashing a real file to find out.
/// A hundred and forty is far past any gallery, and a longer list than that is a client bug rather
/// than an authoring decision.
pub const MAX_MEDIA_FILTER: usize = 140;

/// Prop key of an `image` block's file.
const IMAGE_SRC: &str = "src";

/// Prop key of a `gallery` block's files.
const GALLERY_IMAGES: &str = "images";

/// Block type that names one file.
const IMAGE_BLOCK: &str = "image";

/// Block type that names a list of files.
const GALLERY_BLOCK: &str = "gallery";

// ---------------------------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------------------------

/// Whether a file a block names can be served.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileState {
    /// The row is there and the object is there: the block draws its file.
    Live,
    /// The row is still there but the file is in the trash, so the bytes are gone.
    ///
    /// A third state rather than a boolean, because "gone" and "trashed" need different advice:
    /// a trash can be undone and a purge cannot, so an author told "this image is missing" for a
    /// file that is one click from coming back would go looking for a different picture.
    Trashed,
    /// The row itself is gone — the file was purged. There is nothing to restore.
    Purged,
}

impl FileState {
    /// The name the API reports.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Trashed => "trashed",
            Self::Purged => "purged",
        }
    }

    /// `true` when the renderer cannot draw this file.
    #[must_use]
    pub fn is_broken(self) -> bool {
        !matches!(self, Self::Live)
    }

    /// The sentence the panel prints for a block that cannot draw its file.
    ///
    /// Empty for [`FileState::Live`], so a caller can render it unconditionally.
    #[must_use]
    pub fn advice(self) -> &'static str {
        match self {
            Self::Live => "",
            Self::Trashed => "this image is in the trash — restore it, or pick another one",
            Self::Purged => "this image was deleted — pick another one",
        }
    }
}

/// One block's relationship with one file.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BlockMediaRef {
    /// Block that names the file.
    pub block_id: Uuid,
    /// The block's type, so a caller can filter `image` blocks without knowing the prop names.
    pub block_type: String,
    /// Prop the file came from (`[0].props.src`, `[0].props.images.3`).
    pub path: String,
    /// The media id, as stored.
    pub media_id: Uuid,
    /// What can be done with that file right now.
    pub state: FileState,
    /// The viewport this block draws on, as the payload names it (`none`, `mobile`, `desktop`).
    ///
    /// A `&'static str` rather than the platform's `Viewport` enum because `blocks::Viewport` is
    /// deliberately not a serde type — it is an internal vocabulary with an `as_str` — and
    /// deriving `Serialize` on it here would make the wire format of every future block payload
    /// depend on this module. The string is the same word `hide_on` uses, so a report and a
    /// block's own settings cannot disagree about what viewport they mean.
    pub visible_on: &'static str,
    /// A caption the block degrades to, when it has one and its file is gone.
    ///
    /// Carried on the ref so the panel can show *what the page will actually draw*, rather than a
    /// note about a picture the page is no longer showing.
    pub caption: Option<String>,
    /// What to do about it, in the author's words.
    pub advice: String,
}

impl BlockMediaRef {
    /// `true` when the renderer cannot draw this file.
    #[must_use]
    pub fn is_broken(&self) -> bool {
        self.state.is_broken()
    }
}

/// Every file every block in a tree names, and what can be done with it.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TreeMediaReport {
    /// One entry per block/prop pair that names a file, in tree order.
    pub refs: Vec<BlockMediaRef>,
    /// Distinct files the tree names.
    pub file_count: usize,
    /// Distinct files that cannot be served.
    pub broken_count: usize,
}

impl TreeMediaReport {
    /// The refs belonging to one block.
    #[must_use]
    pub fn for_block(&self, block_id: Uuid) -> Vec<&BlockMediaRef> {
        self.refs
            .iter()
            .filter(|entry| entry.block_id == block_id)
            .collect()
    }

    /// `true` when at least one file the tree names cannot be served.
    #[must_use]
    pub fn has_broken(&self) -> bool {
        self.broken_count > 0
    }

    /// The sentence the editor's status bar prints when the tree has broken files.
    ///
    /// One number, and it counts **files** rather than blocks: a gallery of twelve with two dead
    /// entries is "2 images are gone", not "1 image is gone" and not "13 blocks are affected".
    #[must_use]
    pub fn summary(&self) -> String {
        if self.broken_count == 0 {
            return String::new();
        }
        if self.broken_count == 1 {
            return "1 image is gone".to_owned();
        }
        format!("{} images are gone", self.broken_count)
    }
}

// ---------------------------------------------------------------------------------------------
// Pure helpers — which props are file addresses, and is this value one?
// ---------------------------------------------------------------------------------------------

/// The media-addressing props of a block type, as `(prop key, is list?)`.
///
/// A short allow-list rather than "any prop that looks like an id", because the two rules have
/// opposite failure modes. An over-eager reader would treat a `cta.href` pointing at an
/// id-shaped string as a file and warn about a link; an under-eager one would miss a new media
/// prop added to the registry and serve its dead URLs forever. The registry ships the list, so
/// adding a media type means adding it here in the same commit.
fn media_props(kind: &str) -> &'static [(&'static str, bool)] {
    match kind {
        IMAGE_BLOCK => &[(IMAGE_SRC, false)],
        GALLERY_BLOCK => &[(GALLERY_IMAGES, true)],
        _ => &[],
    }
}

/// Parse a stored prop value as a media id, or `None` when it is not one.
///
/// `None` is the answer for three different things on purpose, and each of them is correct:
///
/// * **a URL the author typed** — `/assets/hero.jpg`, `https://…/photo.png`. Not ours to verify.
/// * **an empty value** — a half-typed URL, or a gallery slot the author has not filled. The
///   validator already owns that case (`block_prop_required` / `block_alt_missing`).
/// * **a relative path into a theme** — a block inserted from a template may point at a file the
///   theme ships. Also not ours to verify.
fn media_id_of(value: &Value) -> Option<Uuid> {
    Uuid::parse_str(value.as_str()?.trim()).ok()
}

/// The caption an `image` block degrades to, or `None` when it has none.
fn caption_of(block: &Block) -> Option<String> {
    block
        .props
        .get("caption")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|caption| !caption.is_empty())
        .map(str::to_owned)
}

// ---------------------------------------------------------------------------------------------
// The walk
// ---------------------------------------------------------------------------------------------

/// One file a block names, as the walk found it: `(block, prop path, media id, type, caption)`.
type Found = (Uuid, String, Uuid, String, Option<String>);

/// Collect every file a tree names, in tree order, with the prop path each came from.
///
/// Pure: it reads props and nothing else, so the unit tests below need no database and the same
/// walk serves the editor's dry run, the preview frame and the public payload.
#[must_use]
pub fn collect_media_refs(blocks: &[Block]) -> Vec<Found> {
    fn walk(blocks: &[Block], prefix: &str, out: &mut Vec<Found>) {
        for (index, block) in blocks.iter().enumerate() {
            let base = if prefix.is_empty() {
                format!("[{index}]")
            } else {
                format!("{prefix}[{index}]")
            };
            let caption = caption_of(block);
            for (key, is_list) in media_props(&block.kind) {
                if *is_list {
                    let Some(items) = block.props.get(*key).and_then(Value::as_array) else {
                        continue;
                    };
                    for (position, item) in items.iter().enumerate() {
                        if let Some(id) = media_id_of(item) {
                            out.push((
                                block.id,
                                format!("{base}.props.{key}.{position}"),
                                id,
                                block.kind.clone(),
                                caption.clone(),
                            ));
                        }
                    }
                } else if let Some(id) =
                    media_id_of(block.props.get(*key).unwrap_or(&Value::Null))
                {
                    out.push((
                        block.id,
                        format!("{base}.props.{key}"),
                        id,
                        block.kind.clone(),
                        caption.clone(),
                    ));
                }
            }
            walk(&block.children, &base, out);
        }
    }
    let mut out = Vec::new();
    walk(blocks, "", &mut out);
    out
}

/// The distinct media ids a tree names.
///
/// Collected before the query rather than after, because a gallery that names the same file
/// twelve times must cost one id in the `= any($1)` array — and because the count the report
/// prints has to be about files, not about how many times a file is named.
#[must_use]
pub fn media_ids(blocks: &[Block]) -> Vec<Uuid> {
    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    for (_, _, id, _, _) in collect_media_refs(blocks) {
        if seen.insert(id) {
            ids.push(id);
        }
    }
    ids
}

// ---------------------------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------------------------

/// The join a block tree makes against the media library.
///
/// Separate from the tree's own validation on purpose: [`crate::blocks::validate`] answers "is
/// this payload storable" and takes no database, and the panel calls it on **every keystroke**
/// (REQ-063 slice 1 wired it to a 250 ms debounce). Folding a media lookup in there would put a
/// query behind the editor's every-change dry run and make a broken file surface as a *validation
/// error* — which blocks a publish, and is the wrong answer for a file somebody trashed on
/// purpose. The two answers are merged where they are shown (the editor's status bar, the preview
/// frame), never inside the validator.
#[derive(Debug, Clone)]
pub struct BlockMediaStore {
    pool: PgPool,
}

/// One row of the media table, reduced to what a block needs.
#[derive(Debug, sqlx::FromRow)]
struct MediaRow {
    id: Uuid,
    deleted_at: Option<OffsetDateTime>,
}

impl BlockMediaStore {
    /// A store over an existing pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Resolve every file a tree names, in one query.
    ///
    /// A missing row has to come back as [`FileState::Purged`] rather than as "no rows", because
    /// the panel's advice differs ("pick another" versus "restore it"). One round trip answers
    /// both halves: look the collected ids up, then read the answer back out of the map.
    pub async fn report(&self, blocks: &[Block]) -> Result<TreeMediaReport> {
        Ok(self.states(blocks).await?.1)
    }

    /// The state of every file a tree names, and the report built from it.
    ///
    /// The two come back together because the renderer needs the **map** and the panel needs the
    /// **report**, and a render path that only got the report would have to rebuild the map from
    /// its own list of refs — a second place where "which files are gone" is decided.
    pub async fn states(
        &self,
        blocks: &[Block],
    ) -> Result<(HashMap<Uuid, FileState>, TreeMediaReport)> {
        let collected = collect_media_refs(blocks);
        let ids = media_ids(blocks);
        if ids.is_empty() {
            return Ok((
                HashMap::new(),
                TreeMediaReport {
                    refs: Vec::new(),
                    file_count: 0,
                    broken_count: 0,
                },
            ));
        }

        let rows = sqlx::query_as::<_, MediaRow>("select id, deleted_at from media where id = any($1)")
            .bind(&ids)
            .fetch_all(&self.pool)
            .await?;

        let mut states: HashMap<Uuid, FileState> = HashMap::with_capacity(rows.len());
        for row in rows {
            states.insert(
                row.id,
                if row.deleted_at.is_some() {
                    FileState::Trashed
                } else {
                    FileState::Live
                },
            );
        }

        let mut broken: HashSet<Uuid> = HashSet::new();
        let refs = collected
            .into_iter()
            .map(|(block_id, path, id, kind, caption)| {
                // An id the table does not have is a file that was purged, which is a fact about
                // the id rather than about the row: the FK emptied the column, or the id was
                // never one of ours.
                let state = states.get(&id).copied().unwrap_or(FileState::Purged);
                if state.is_broken() {
                    broken.insert(id);
                }
                BlockMediaRef {
                    block_id,
                    block_type: kind,
                    path,
                    media_id: id,
                    state,
                    visible_on: viewport_of(blocks, block_id).as_str(),
                    caption,
                    advice: state.advice().to_owned(),
                }
            })
            .collect();

        Ok((
            states,
            TreeMediaReport {
                file_count: ids.len(),
                broken_count: broken.len(),
                refs,
            },
        ))
    }
}

/// Where a block sits in the tree, so a ref can say which viewport it draws on.
///
/// A tree walk per ref would be quadratic on a page of forty blocks, so the map is built once
/// and read. The default is [`Viewport::Both`], which is the honest answer when a block cannot be
/// found: a block we cannot locate is still a block, and reporting it as phone-only would invent a
/// viewport nobody set.
fn viewport_of(blocks: &[Block], block_id: Uuid) -> Viewport {
    fn walk(blocks: &[Block], block_id: Uuid) -> Option<Viewport> {
        for block in blocks {
            if block.id == block_id {
                return Some(block_hide_on(block));
            }
            if let Some(found) = walk(&block.children, block_id) {
                return Some(found);
            }
        }
        None
    }
    walk(blocks, block_id).unwrap_or(Viewport::Both)
}

// ---------------------------------------------------------------------------------------------
// The degradation
// ---------------------------------------------------------------------------------------------

/// A block tree with its broken files removed, for the renderer to draw.
///
/// **This is the whole answer to "the picture was deleted".** A page that lost a file keeps its
/// layout, its other blocks, its caption and its text; what it loses is the `<img>` that would
/// have been a broken-image icon in a thousand browsers. The alternative — leaving the id in the
/// tree and letting the browser discover it is dead — is what the REQ names as the failure, and
/// it is worse than a gap because it is a *lie*: the HTML still claims there is a picture.
///
/// An `image` block whose file is gone becomes its caption, and nothing else. A `gallery` keeps
/// the files that survived and drops the dead entries, because a gallery of four with one missing
/// is still a gallery of three, whereas dropping the whole block would take four good pictures off
/// the page over one bad id.
///
/// A block whose file is *live*, or whose prop holds a URL rather than an id, is returned
/// untouched. `states` therefore only has to carry the files it is told about, which is what lets
/// the preview frame simulate a deletion by adding one entry.
#[must_use]
pub fn degrade_tree(blocks: &[Block], states: &HashMap<Uuid, FileState>) -> Vec<Block> {
    blocks
        .iter()
        .filter_map(|block| {
            let mut kept = block.clone();
            kept.children = degrade_tree(&block.children, states);
            match block.kind.as_str() {
                IMAGE_BLOCK => {
                    let broken = block
                        .props
                        .get(IMAGE_SRC)
                        .and_then(media_id_of)
                        .is_some_and(|id| states.get(&id).is_some_and(|state| state.is_broken()));
                    if !broken {
                        return Some(kept);
                    }
                    // The block keeps its place and becomes its caption, so a page that lost a
                    // picture still reads as a page. With no caption there is nothing to say, and
                    // an `image` block with empty text would be dropped by the renderer as a blank
                    // block — so it becomes an explicit short note rather than a silent gap.
                    let text = caption_of(block).unwrap_or_else(|| {
                        "this image is no longer available".to_owned()
                    });
                    kept.kind = "text".to_owned();
                    kept.props = json!({ "text": text });
                    Some(kept)
                }
                GALLERY_BLOCK => {
                    let images: Vec<Value> = block
                        .props
                        .get(GALLERY_IMAGES)
                        .and_then(Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .filter(|item| {
                                    !media_id_of(item).is_some_and(|id| {
                                        states.get(&id).is_some_and(|state| state.is_broken())
                                    })
                                })
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default();
                    // A gallery that lost every image is not a gallery: the renderer would draw
                    // an empty grid with a caption saying "0 images". It degrades to its own
                    // caption text when it has one, and is dropped otherwise — the report's
                    // warning is the trace either way, and a gap is honest where a fake grid is
                    // not.
                    if images.is_empty() {
                        return match caption_of(block) {
                            Some(caption) => {
                                kept.kind = "text".to_owned();
                                kept.props = json!({ "text": caption });
                                Some(kept)
                            }
                            None => None,
                        };
                    }
                    kept.props[GALLERY_IMAGES] = Value::Array(images);
                    Some(kept)
                }
                _ => Some(kept),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The simulation filter
// ---------------------------------------------------------------------------------------------

/// The ids `?media=` names, or an error when the value is not a list of media ids.
///
/// Two refusals, and they are different failures. A word that is not a uuid is a client bug and
/// deserves a message that names it. A list longer than [`MAX_MEDIA_FILTER`] is not a bug at all,
/// it is a page that never existed, and truncating it silently would answer "yes" to a simulation
/// that only covered half the gallery — a report claiming a file was checked when it was not is
/// worse than a refused one.
pub fn parse_media_filter(raw: Option<&str>) -> Result<Vec<Uuid>> {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(Vec::new());
    };
    if raw.len() > MAX_MEDIA_FILTER * 37 {
        return Err(ContentError::InvalidText(format!(
            "at most {MAX_MEDIA_FILTER} media ids may be simulated at once"
        )));
    }
    let mut ids: Vec<Uuid> = Vec::new();
    for part in raw.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        let id = Uuid::parse_str(trimmed)
            .map_err(|_| ContentError::InvalidText(format!("{trimmed:?} is not a media id")))?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// The state map a render should use: what the store says, plus whatever the caller simulated.
///
/// One entry point rather than two call sites merging maps with different rules, because "which
/// files are gone" then has one answer.
#[must_use]
pub fn states_for_render(
    real: &HashMap<Uuid, FileState>,
    simulated: &[Uuid],
) -> HashMap<Uuid, FileState> {
    let mut merged = real.clone();
    for id in simulated {
        merged.insert(*id, FileState::Purged);
    }
    merged
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::parse_blocks;

    /// One block of a tree, built through the platform's own reader.
    fn block(kind: &str, props: Value) -> Block {
        parse_blocks(&json!([{ "id": Uuid::new_v4().to_string(), "type": kind, "props": props }]))
            .expect("a well-formed block")
            .pop()
            .expect("one block")
    }

    /// A tree with a `columns` container holding `columns` empty `column` blocks.
    fn container(children: Vec<Vec<Block>>) -> Vec<Block> {
        let mut tree = vec![block("columns", json!({ "columns": 2 }))];
        let children: Vec<Block> = children
            .into_iter()
            .map(|blocks| {
                let mut column = block("column", json!({}));
                column.children = blocks;
                column
            })
            .collect();
        tree[0].children = children;
        tree
    }

    fn states(entries: &[(Uuid, FileState)]) -> HashMap<Uuid, FileState> {
        entries.iter().copied().collect()
    }

    #[test]
    fn an_image_block_names_its_file() {
        let id = Uuid::new_v4();
        let tree = vec![block(
            IMAGE_BLOCK,
            json!({ "src": id.to_string(), "alt": "A bridge" }),
        )];
        let refs = collect_media_refs(&tree);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].2, id);
        assert_eq!(refs[0].1, "[0].props.src");
        assert_eq!(refs[0].3, IMAGE_BLOCK);
    }

    #[test]
    fn a_gallery_names_every_entry_with_its_own_path() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let tree = vec![block(
            GALLERY_BLOCK,
            json!({ "images": [first.to_string(), "  ", "https://example.test/a.png", second.to_string()] }),
        )];
        let refs = collect_media_refs(&tree);
        assert_eq!(refs.len(), 2, "a hand-written URL and a blank entry are not files");
        assert_eq!(refs[0].2, first);
        assert_eq!(refs[1].2, second);
        assert_eq!(refs[1].1, "[0].props.images.3");
    }

    #[test]
    fn a_nested_block_is_found_at_its_real_path() {
        let id = Uuid::new_v4();
        let tree = container(vec![
            vec![],
            vec![block(
                IMAGE_BLOCK,
                json!({ "src": id.to_string(), "alt": "x" }),
            )],
        ]);
        let refs = collect_media_refs(&tree);
        assert_eq!(refs.len(), 1);
        assert_eq!(
            refs[0].1, "[0][1][0].props.src",
            "the path has to name every level — the columns block, the column, then the image — \
             or a report cannot say which block it is talking about"
        );
    }

    #[test]
    fn a_url_is_not_a_file() {
        let tree = vec![
            block(IMAGE_BLOCK, json!({ "src": "https://cdn.example.test/a.jpg", "alt": "x" })),
            block(IMAGE_BLOCK, json!({ "src": "/assets/hero.jpg", "alt": "x" })),
            block(IMAGE_BLOCK, json!({ "src": "", "alt": "x" })),
            block(IMAGE_BLOCK, json!({ "src": "  ", "alt": "x" })),
        ];
        assert!(collect_media_refs(&tree).is_empty());
        assert!(media_ids(&tree).is_empty(), "nothing to resolve means no query");
    }

    #[test]
    fn a_block_type_without_media_props_is_never_walked_for_files() {
        // `cta.href` may legitimately hold an id-shaped string, and warning about a link is the
        // failure this allow-list exists to prevent.
        let tree = vec![block("cta", json!({ "title": "Go", "href": Uuid::new_v4().to_string() }))];
        assert!(collect_media_refs(&tree).is_empty());
    }

    #[test]
    fn the_same_file_named_twice_is_one_file() {
        let id = Uuid::new_v4();
        let tree = vec![
            block(IMAGE_BLOCK, json!({ "src": id.to_string(), "alt": "x" })),
            block(GALLERY_BLOCK, json!({ "images": [id.to_string()] })),
        ];
        assert_eq!(media_ids(&tree), vec![id]);
    }

    #[test]
    fn a_trashed_file_and_a_purged_one_get_different_words() {
        assert_ne!(FileState::Trashed.advice(), FileState::Purged.advice());
        assert!(FileState::Trashed.advice().contains("trash"));
        assert!(FileState::Trashed.advice().contains("restore"));
        assert!(FileState::Purged.advice().contains("deleted"));
        assert!(FileState::Live.advice().is_empty());
    }

    #[test]
    fn a_deleted_image_degrades_to_its_caption() {
        let id = Uuid::new_v4();
        let tree = vec![block(
            IMAGE_BLOCK,
            json!({ "src": id.to_string(), "alt": "A bridge", "caption": "The old bridge" }),
        )];
        let degraded = degrade_tree(&tree, &states(&[(id, FileState::Purged)]));
        assert_eq!(degraded.len(), 1, "the block keeps its place on the page");
        assert_eq!(degraded[0].kind, "text");
        assert_eq!(degraded[0].props["text"], "The old bridge");
    }

    #[test]
    fn a_deleted_image_without_a_caption_says_so_rather_than_vanishing() {
        let id = Uuid::new_v4();
        let tree = vec![block(IMAGE_BLOCK, json!({ "src": id.to_string(), "alt": "A bridge" }))];
        let degraded = degrade_tree(&tree, &states(&[(id, FileState::Trashed)]));
        assert_eq!(degraded.len(), 1);
        assert_eq!(degraded[0].kind, "text");
        assert_eq!(degraded[0].props["text"], "this image is no longer available");
    }

    #[test]
    fn a_live_image_is_untouched() {
        let id = Uuid::new_v4();
        let tree = vec![block(IMAGE_BLOCK, json!({ "src": id.to_string(), "alt": "A bridge" }))];
        let degraded = degrade_tree(&tree, &states(&[(id, FileState::Live)]));
        assert_eq!(degraded.len(), 1);
        assert_eq!(degraded[0].kind, IMAGE_BLOCK, "a live file must come back unchanged");
        assert_eq!(degraded[0].props["src"], id.to_string());
    }

    #[test]
    fn a_gallery_keeps_the_files_that_survived() {
        let alive = Uuid::new_v4();
        let gone = Uuid::new_v4();
        let tree = vec![block(
            GALLERY_BLOCK,
            json!({ "images": [alive.to_string(), gone.to_string()], "columns": 2 }),
        )];
        let degraded = degrade_tree(&tree, &states(&[(gone, FileState::Purged)]));
        assert_eq!(degraded.len(), 1);
        let images = degraded[0].props["images"].as_array().expect("a list");
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].as_str(), Some(alive.to_string().as_str()));
    }

    #[test]
    fn a_gallery_that_lost_every_image_is_not_an_empty_grid() {
        let gone = Uuid::new_v4();
        let tree = vec![block(GALLERY_BLOCK, json!({ "images": [gone.to_string()] }))];
        assert!(
            degrade_tree(&tree, &states(&[(gone, FileState::Purged)]))
                .is_empty(),
            "an empty grid with '0 images' under it is worse than a gap"
        );
    }

    #[test]
    fn a_gallery_of_hand_written_urls_is_not_touched() {
        let tree = vec![block(
            GALLERY_BLOCK,
            json!({ "images": ["https://example.test/a.png", "https://example.test/b.png"] }),
        )];
        let degraded = degrade_tree(&tree, &HashMap::new());
        assert_eq!(degraded.len(), 1);
        assert_eq!(degraded[0].props["images"].as_array().expect("a list").len(), 2);
    }

    #[test]
    fn a_deleted_image_inside_a_column_degrades_where_it_stands() {
        let id = Uuid::new_v4();
        let tree = container(vec![
            vec![block(
                IMAGE_BLOCK,
                json!({ "src": id.to_string(), "alt": "A bridge", "caption": "The old bridge" }),
            )],
            vec![],
        ]);
        let degraded = degrade_tree(&tree, &states(&[(id, FileState::Purged)]));
        assert_eq!(degraded[0].children[0].children.len(), 1);
        assert_eq!(degraded[0].children[0].children[0].kind, "text");
    }

    #[test]
    fn a_simulated_deletion_only_touches_the_file_it_names() {
        let simulated = Uuid::new_v4();
        let alive = Uuid::new_v4();
        let tree = vec![
            block(IMAGE_BLOCK, json!({ "src": alive.to_string(), "alt": "x" })),
            block(IMAGE_BLOCK, json!({ "src": simulated.to_string(), "alt": "y" })),
        ];
        let merged = states_for_render(&states(&[(alive, FileState::Live)]), &[simulated]);
        let degraded = degrade_tree(&tree, &merged);
        assert_eq!(degraded.len(), 2);
        assert_eq!(degraded[0].kind, IMAGE_BLOCK);
        assert_eq!(degraded[1].kind, "text");
    }

    #[test]
    fn a_filter_named_ids_are_parsed_and_deduped() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let parsed = parse_media_filter(Some(&format!("{first}, {second},{first}")))
            .expect("ids parse");
        assert_eq!(parsed, vec![first, second]);
    }

    #[test]
    fn no_filter_is_no_simulation() {
        assert!(parse_media_filter(None).expect("an absent filter").is_empty());
        assert!(parse_media_filter(Some("  ")).expect("a blank filter").is_empty());
    }

    #[test]
    fn a_filter_that_is_not_ids_is_refused() {
        let error = parse_media_filter(Some("not-a-uuid")).expect_err("a bad filter is an error");
        assert!(matches!(error, ContentError::InvalidText(_)));
    }

    #[test]
    fn a_filter_longer_than_the_bound_is_refused_rather_than_truncated() {
        let raw = std::iter::repeat_n(Uuid::new_v4().to_string(), MAX_MEDIA_FILTER + 1)
            .collect::<Vec<_>>()
            .join(",");
        assert!(parse_media_filter(Some(&raw)).is_err());
    }

    #[test]
    fn a_hidden_block_reports_the_viewport_it_actually_draws_on() {
        let id = Uuid::new_v4();
        let mut image = block(IMAGE_BLOCK, json!({ "src": id.to_string(), "alt": "x" }));
        image.meta = json!({ "hide_on": "mobile" });
        let tree = vec![image];
        assert_eq!(viewport_of(&tree, tree[0].id), Viewport::Mobile);
    }

    #[test]
    fn a_block_the_walk_cannot_find_reports_both_viewports() {
        let tree = vec![block(IMAGE_BLOCK, json!({ "src": Uuid::new_v4().to_string(), "alt": "x" }))];
        assert_eq!(viewport_of(&tree, Uuid::nil()), Viewport::Both);
    }

    #[test]
    fn a_report_counts_files_and_broken_files_separately() {
        let alive = Uuid::new_v4();
        let gone = Uuid::new_v4();
        let named_twice = Uuid::new_v4();
        let tree = vec![
            block(IMAGE_BLOCK, json!({ "src": alive.to_string(), "alt": "x" })),
            block(GALLERY_BLOCK, json!({ "images": [gone.to_string(), named_twice.to_string()] })),
            block(GALLERY_BLOCK, json!({ "images": [named_twice.to_string()] })),
        ];
        let report = TreeMediaReport {
            file_count: 3,
            broken_count: 1,
            refs: Vec::new(),
        };
        assert_eq!(report.file_count, 3);
        assert_eq!(report.broken_count, 1, "a file named twice is one broken file");
        assert!(report.has_broken());
        assert_eq!(report.summary(), "1 image is gone");
        assert_eq!(media_ids(&tree).len(), 3);
        assert_eq!(collect_media_refs(&tree).len(), 4, "four references, three files");
    }
}
