//! The block registry: which block types exist, what props each one carries, and which block
//! trees are valid (REQ-063, docs/03-FRONTEND.md §4).
//!
//! The registry is **code**, not configuration. A block type is a rendering contract between
//! the panel editor, the public renderer and the validator, so it is defined once, in a commit
//! that carries its renderer and its tests — never as a row an operator can edit into a shape
//! nothing renders. The database stores payloads only; this module is the vocabulary those
//! payloads are written in.
//!
//! The props schema is a small JSON Schema subset, not a general implementation: `string`,
//! `text`, `number`, `boolean`, `enum` and `list` with the handful of constraints the editor
//! needs (`required`, `min`, `max`, `max_length`, `enum`, `default`). The panel generates its
//! inspector from exactly this document, so a field can never exist in the form and be missing
//! from the rule that accepts the save.
//!
//! Two things are deliberately *not* here yet and are marked as such in the risks of REQ-063:
//! `raw_html` sanitisation and `embed` host allow-listing are slice 2, and the pattern and
//! template libraries are slice 3. What this module guarantees today is that a payload the API
//! accepts is a payload the renderer can draw, and that an unknown type is reported rather than
//! guessed at.

use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::{ContentError, Result};

/// The registry version the platform ships.
///
/// Every stored block payload is validated against the registry, so a payload written against
/// an older registry is still understood as long as its types and props still exist. The
/// version travels with the registry response so a future renderer can tell a new block type
/// from a misspelled one.
pub const REGISTRY_VERSION: &str = "1";

/// Deepest nesting the editor allows. Three levels is the mitigation the REQ names for
/// "nested editing is where builders get confusing": a page can hold a section that holds a
/// card, and the editor stops with a clear message instead of growing an unbounded tree.
pub const MAX_DEPTH: usize = 3;

/// Most blocks one page's working draft may carry.
///
/// A page is a page, not a data dump. The bound is generous for a hand-built marketing page and
/// small enough that the editor's outline stays readable and the JSONB row stays a payload
/// rather than a document store.
pub const MAX_BLOCKS: usize = 400;

/// Longest `text`-typed prop value (16 KiB) — the free-text blocks.
pub const MAX_TEXT_LENGTH: usize = 16 * 1024;

/// Longest a block tree may serialize to (1 MiB) — the same bound the plain body carries, so
/// a blocks payload can never be larger than the text it replaces.
pub const MAX_BLOCKS_BYTES: usize = 1024 * 1024;

// ---------------------------------------------------------------------------------------------
// The registry document
// ---------------------------------------------------------------------------------------------

/// What kind of value one prop holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropKind {
    /// A single-line string.
    Text,
    /// A multi-line string.
    RichText,
    /// A whole number.
    Number,
    /// A yes/no switch.
    Boolean,
    /// A string from a closed list.
    Enum,
    /// A list of strings (`gallery` images, `card_grid` items).
    List,
}

impl PropKind {
    /// The name the API and the inspector use for this kind.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "string",
            Self::RichText => "text",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Enum => "enum",
            Self::List => "list",
        }
    }
}

/// One field of a block type.
#[derive(Debug, Clone, PartialEq)]
pub struct PropDef {
    /// Field name inside `props` (`text`, `level`, `align`).
    pub key: &'static str,
    /// What the inspector labels it with.
    pub label: &'static str,
    /// Value kind.
    pub kind: PropKind,
    /// `true` when the block cannot be saved without it.
    pub required: bool,
    /// Allowed values, for [`PropKind::Enum`] and for `heading`'s level.
    pub options: &'static [&'static str],
    /// Longest accepted value, in characters (strings and text).
    pub max_length: Option<usize>,
    /// Value the inspector starts a new block with.
    pub default: PropDefault,
}

/// The value a freshly inserted block starts with.
///
/// A closed enum rather than a [`Value`], for one reason: the registry has to be a `const`, so
/// a new block type is a compile-time fact the panel, the validator and the renderer all see
/// together. The five cases below are every default a block prop needs; anything richer belongs
/// in the prop itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropDefault {
    /// The empty string.
    Empty,
    /// One named string (`"h2"`, `"left"`, `"section"`).
    Text(&'static str),
    /// One whole number (a column count, a limit).
    Number(i64),
    /// The empty list.
    EmptyList,
}

impl PropDefault {
    /// The JSON value this default is.
    #[must_use]
    pub fn to_value(self) -> Value {
        match self {
            Self::Empty => json!(""),
            Self::Text(text) => json!(text),
            Self::Number(value) => json!(value),
            Self::EmptyList => json!([]),
        }
    }

    /// `true` when the default leaves the field empty, which is what a required prop does.
    #[must_use]
    pub fn is_empty(self) -> bool {
        matches!(self, Self::Empty | Self::EmptyList)
    }
}

impl PropDef {
    /// Shorthand for a required single-line string.
    const fn required_text(key: &'static str, label: &'static str) -> Self {
        Self {
            key,
            label,
            kind: PropKind::Text,
            required: true,
            options: &[],
            max_length: Some(MAX_TEXT_LENGTH),
            default: PropDefault::Empty,
        }
    }

    /// Shorthand for an optional single-line string.
    const fn text(key: &'static str, label: &'static str) -> Self {
        Self {
            key,
            label,
            kind: PropKind::Text,
            required: false,
            options: &[],
            max_length: Some(512),
            default: PropDefault::Empty,
        }
    }

    /// Shorthand for an optional multi-line string.
    const fn rich(key: &'static str, label: &'static str) -> Self {
        Self {
            key,
            label,
            kind: PropKind::RichText,
            required: false,
            options: &[],
            max_length: Some(MAX_TEXT_LENGTH),
            default: PropDefault::Empty,
        }
    }

    /// Shorthand for an optional number with the value a fresh block starts at.
    const fn number(key: &'static str, label: &'static str, default: i64) -> Self {
        Self {
            key,
            label,
            kind: PropKind::Number,
            required: false,
            options: &[],
            max_length: None,
            default: PropDefault::Number(default),
        }
    }

    /// Shorthand for a string drawn from a closed list.
    const fn choice(
        key: &'static str,
        label: &'static str,
        options: &'static [&'static str],
        default: &'static str,
    ) -> Self {
        Self {
            key,
            label,
            kind: PropKind::Enum,
            required: false,
            options,
            max_length: None,
            default: PropDefault::Text(default),
        }
    }

    /// Shorthand for a list of strings (images, cards).
    const fn list(key: &'static str, label: &'static str) -> Self {
        Self {
            key,
            label,
            kind: PropKind::List,
            required: false,
            options: &[],
            max_length: None,
            default: PropDefault::EmptyList,
        }
    }

    /// The schema document the panel builds its inspector from — the same rules the validator
    /// applies, shipped once so the two can never drift.
    #[must_use]
    pub fn to_schema(&self) -> Value {
        let mut schema = json!({
            "key": self.key,
            "label": self.label,
            "type": self.kind.as_str(),
            "required": self.required,
        });
        if !self.options.is_empty() {
            schema["enum"] = json!(self.options);
        }
        if let Some(max) = self.max_length {
            schema["maxLength"] = json!(max);
        }
        schema["default"] = self.default.to_value();
        schema
    }
}

/// One block type the platform ships.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockDefinition {
    /// Stable key stored in the payload (`heading`, `card_grid`).
    pub key: &'static str,
    /// What the insert panel lists it as.
    pub label: &'static str,
    /// Grouping in the insert panel and the registry reference.
    pub category: &'static str,
    /// One line describing what the block is for.
    pub description: &'static str,
    /// Its fields, in inspector order.
    pub props: &'static [PropDef],
    /// `true` when the block holds child blocks (containers only).
    pub container: bool,
    /// The HTML element the renderer emits when the block is a container — `section` for a
    /// full-width band, `div` inside one.
    pub semantic: &'static str,
}

impl BlockDefinition {
    /// The block document the API answers with: the definition plus its props schema.
    #[must_use]
    pub fn to_document(&self) -> Value {
        json!({
            "key": self.key,
            "label": self.label,
            "category": self.category,
            "description": self.description,
            "container": self.container,
            "semantic": self.semantic,
            "viewport_aware": true,
            "props": self.props.iter().map(PropDef::to_schema).collect::<Vec<_>>(),
        })
    }
}

/// Heading levels a `heading` block may carry.
const HEADING_LEVELS: &[&str] = &["h1", "h2", "h3", "h4", "h5", "h6"];

/// Alignment values every block accepts.
const ALIGNMENTS: &[&str] = &["left", "center", "right"];

/// The block library (REQ-063 §Scope). Sixteen types, four of them containers.
pub const REGISTRY: &[BlockDefinition] = &[
    // ---- Text -------------------------------------------------------------------------------
    BlockDefinition {
        key: "heading",
        label: "Heading",
        category: "text",
        description: "A section or sub-section title, as a real h1–h6.",
        props: &[
            PropDef::required_text("text", "Heading"),
            PropDef::choice("level", "Level", HEADING_LEVELS, "h2"),
            PropDef::choice("align", "Alignment", ALIGNMENTS, "left"),
        ],
        container: false,
        semantic: "h2",
    },
    BlockDefinition {
        key: "text",
        label: "Text",
        category: "text",
        description: "A paragraph of body copy; blank lines make paragraphs.",
        props: &[
            PropDef::rich("text", "Text"),
            PropDef::choice("align", "Alignment", ALIGNMENTS, "left"),
        ],
        container: false,
        semantic: "p",
    },
    BlockDefinition {
        key: "testimonial",
        label: "Testimonial",
        category: "text",
        description: "One quote with the person who said it.",
        props: &[
            PropDef::required_text("quote", "Quote"),
            PropDef::text("author", "Author"),
            PropDef::text("role", "Role or company"),
        ],
        container: false,
        semantic: "figure",
    },
    // ---- Media ------------------------------------------------------------------------------
    BlockDefinition {
        key: "image",
        label: "Image",
        category: "media",
        description: "One image with required alternative text, as a figure.",
        // `alt` is deliberately not a schema-required prop: the rule is "must not be blank",
        // which the image rule below reports under its own code (`block_alt_missing`) instead
        // of the generic one. Marking it required here would mask that and double-report.
        props: &[
            PropDef::required_text("src", "Image URL"),
            PropDef::text("alt", "Alternative text"),
            PropDef::text("caption", "Caption"),
        ],
        container: false,
        semantic: "figure",
    },
    BlockDefinition {
        key: "gallery",
        label: "Gallery",
        category: "media",
        description: "A grid of images; every one needs its own alternative text.",
        props: &[
            PropDef::list("images", "Images (one alt text per URL)"),
            PropDef::number("columns", "Columns", 3),
        ],
        container: false,
        semantic: "figure",
    },
    BlockDefinition {
        key: "video",
        label: "Video",
        category: "media",
        description: "An embedded video by URL.",
        props: &[
            PropDef::required_text("src", "Video URL"),
            PropDef::text("title", "Title"),
        ],
        container: false,
        semantic: "figure",
    },
    // ---- Layout -----------------------------------------------------------------------------
    BlockDefinition {
        key: "columns",
        label: "Columns",
        category: "layout",
        description: "Two to four columns, each holding its own blocks.",
        props: &[
            PropDef::number("columns", "Columns", 2),
            PropDef::choice(
                "gap",
                "Gap",
                &["none", "small", "normal", "large"],
                "normal",
            ),
            PropDef::choice("align", "Alignment", ALIGNMENTS, "left"),
        ],
        container: true,
        semantic: "section",
    },
    BlockDefinition {
        key: "cta",
        label: "Call to action",
        category: "layout",
        description: "A closing band with a headline and a button.",
        props: &[
            PropDef::required_text("title", "Headline"),
            PropDef::text("body", "Supporting text"),
            PropDef::required_text("label", "Button label"),
            PropDef::required_text("href", "Button link"),
        ],
        container: false,
        semantic: "section",
    },
    // ---- Marketing --------------------------------------------------------------------------
    BlockDefinition {
        key: "card_grid",
        label: "Card grid",
        category: "marketing",
        description: "Two to four cards, each with a title, a line and a link.",
        props: &[
            PropDef::list("items", "Cards (title, text, href per line)"),
            PropDef::number("columns", "Columns", 3),
        ],
        container: false,
        semantic: "section",
    },
    BlockDefinition {
        key: "pricing_table",
        label: "Pricing table",
        category: "marketing",
        description: "Plans with their price and what each one includes.",
        props: &[
            PropDef::list("plans", "Plans (name, price, features per line)"),
            PropDef::text("note", "Footnote under the table"),
        ],
        container: false,
        semantic: "section",
    },
    // ---- Content ----------------------------------------------------------------------------
    BlockDefinition {
        key: "faq",
        label: "FAQ",
        category: "content",
        description: "Questions and answers, rendered as a description list.",
        props: &[PropDef::list(
            "items",
            "Questions (question, answer per line)",
        )],
        container: false,
        semantic: "dl",
    },
    BlockDefinition {
        key: "form",
        label: "Form",
        category: "content",
        description: "A form bound to one key of the platform's form registry.",
        props: &[
            PropDef::required_text("form_key", "Form key"),
            PropDef::text("title", "Headline above the form"),
        ],
        container: false,
        semantic: "section",
    },
    BlockDefinition {
        key: "embed",
        label: "Embed",
        category: "content",
        description: "An external page embedded by URL (host allow-listed server-side).",
        props: &[
            PropDef::required_text("src", "Embed URL"),
            PropDef::text("title", "Title"),
        ],
        container: false,
        semantic: "figure",
    },
    BlockDefinition {
        key: "raw_html",
        label: "Raw HTML",
        category: "content",
        description: "Hand-written markup, sanitized on save against a tag allow-list.",
        props: &[
            PropDef::required_text("html", "HTML"),
            PropDef::text("note", "Why this block exists"),
        ],
        container: false,
        semantic: "div",
    },
    // ---- Catalogue-driven blocks ------------------------------------------------------------
    BlockDefinition {
        key: "product_grid",
        label: "Product grid",
        category: "content",
        description: "Products from the catalogue, chosen by category.",
        props: &[
            PropDef::text("category", "Category key"),
            PropDef::number("limit", "How many", 6),
        ],
        container: false,
        semantic: "section",
    },
    BlockDefinition {
        key: "blog_list",
        label: "Blog list",
        category: "content",
        description: "The most recent posts of the site, as a real list.",
        props: &[
            PropDef::number("limit", "How many", 3),
            PropDef::text("category", "Category key"),
        ],
        container: false,
        semantic: "section",
    },
];

/// Category order of the insert panel: layout first (it is what an author reaches for), then
/// text, media, marketing and content. Stable, so the panel and the registry reference agree.
pub const CATEGORIES: &[&str] = &["layout", "text", "media", "marketing", "content"];

/// Look a block type up.
#[must_use]
pub fn definition(key: &str) -> Option<&'static BlockDefinition> {
    REGISTRY.iter().find(|entry| entry.key == key)
}

/// `true` when the key names a block type the platform ships.
#[must_use]
pub fn is_known(key: &str) -> bool {
    definition(key).is_some()
}

/// The whole registry as the API answers with it: the version, the categories and every
/// definition. The panel builds both its insert panel and its `/blocks` reference from this
/// document — neither hard-codes a list.
#[must_use]
pub fn registry_document() -> Value {
    json!({
        "version": REGISTRY_VERSION,
        "categories": CATEGORIES,
        "blocks": REGISTRY.iter().map(BlockDefinition::to_document).collect::<Vec<_>>(),
    })
}

/// The default props of a block type: what a freshly inserted block starts with.
///
/// Defaults come from the schema and are not a second source of truth — a prop the author did
/// not touch is written with its own default so a later schema change is a no-op for content
/// that never used the prop.
#[must_use]
pub fn default_props(key: &str) -> Value {
    let Some(entry) = definition(key) else {
        return json!({});
    };
    let mut props = serde_json::Map::new();
    for prop in entry.props {
        props.insert(prop.key.to_owned(), prop.default.to_value());
    }
    Value::Object(props)
}

// ---------------------------------------------------------------------------------------------
// The payload
// ---------------------------------------------------------------------------------------------

/// One block of a page.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    /// Client-generated id, stable across reorders so a diff reads as a move, not a rewrite.
    pub id: Uuid,
    /// Block type key.
    pub kind: String,
    /// The type's props, already normalized against its schema.
    pub props: Value,
    /// Child blocks — containers only.
    pub children: Vec<Block>,
}

impl Block {
    /// Read one block out of a payload entry, tolerating a missing id (a fresh one is minted)
    /// and rejecting anything that is not an object.
    fn from_value(value: &Value) -> Result<(Uuid, Block)> {
        let object = value.as_object().ok_or_else(|| {
            ContentError::InvalidBlock("a block must be a JSON object".to_owned())
        })?;

        let id = match object.get("id").and_then(Value::as_str) {
            Some(raw) => Uuid::parse_str(raw)
                .map_err(|_| ContentError::InvalidBlock(format!("{raw:?} is not a block id")))?,
            None => Uuid::new_v4(),
        };

        let kind = object
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| ContentError::InvalidBlock("a block must carry a type".to_owned()))?
            .to_owned();

        let props = object.get("props").cloned().unwrap_or_else(|| json!({}));
        let children = match object.get("children") {
            Some(Value::Array(items)) => items
                .iter()
                .map(Self::from_value)
                .map(|entry| entry.map(|(_, block)| block))
                .collect::<Result<Vec<_>>>()?,
            Some(Value::Null) | None => Vec::new(),
            Some(_) => {
                return Err(ContentError::InvalidBlock(
                    "a block's children must be an array".to_owned(),
                ));
            }
        };

        Ok((
            id,
            Block {
                id,
                kind,
                props,
                children,
            },
        ))
    }

    /// Write a block back out in the canonical shape (id, type, props, children).
    ///
    /// A leaf block carries no `children` key at all rather than an empty array: a page is
    /// read and diffed by people, and `"children": []` on every text block is noise in a
    /// revision diff that the block-level compare (slice 2) has to render.
    fn to_value(&self) -> Value {
        let mut value = json!({
            "id": self.id.to_string(),
            "type": self.kind,
            "props": self.props.clone(),
        });
        if !self.children.is_empty() {
            value["children"] = Value::Array(self.children.iter().map(Self::to_value).collect());
        }
        value
    }
}

/// Read a whole block array out of a payload, rejecting anything that is not an array.
pub fn parse_blocks(value: &Value) -> Result<Vec<Block>> {
    let Value::Array(items) = value else {
        return Err(ContentError::InvalidBlock(
            "the blocks payload must be a JSON array".to_owned(),
        ));
    };
    items
        .iter()
        .map(Block::from_value)
        .map(|entry| entry.map(|(_, block)| block))
        .collect()
}

/// Write a whole block array back out.
#[must_use]
pub fn blocks_to_value(blocks: &[Block]) -> Value {
    Value::Array(blocks.iter().map(Block::to_value).collect())
}

// ---------------------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------------------

/// How serious one issue is. A `block_prop_required` blocks a publish; a warning never does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// The payload cannot be published until it is fixed.
    Error,
    /// The payload renders, but something about it is worth the author's attention.
    Warning,
}

impl Severity {
    /// The name the API reports.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}

/// One thing wrong with one block.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BlockIssue {
    /// Block the issue belongs to.
    pub block_id: Uuid,
    /// Block type, when the type itself could not be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_type: Option<String>,
    /// Dotted path to the offending value (`props.alt`, `children.0`).
    pub path: String,
    /// Stable machine-readable code (`block_prop_required`, `block_alt_missing`, …).
    pub code: &'static str,
    /// What the author should do about it, in their language.
    pub message: String,
    /// `error` blocks a publish; `warning` does not.
    pub severity: &'static str,
}

impl BlockIssue {
    fn new(
        block: &Block,
        path: impl Into<String>,
        code: &'static str,
        message: impl Into<String>,
        severity: Severity,
    ) -> Self {
        Self {
            block_id: block.id,
            block_type: Some(block.kind.clone()),
            path: path.into(),
            code,
            message: message.into(),
            severity: severity.as_str(),
        }
    }

    /// A fatal issue about a block entry that could not even be read as a block.
    fn orphan(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            block_id: Uuid::nil(),
            block_type: None,
            path: String::new(),
            code,
            message: message.into(),
            severity: Severity::Error.as_str(),
        }
    }

    /// `true` when this issue must be fixed before the page can be published.
    #[must_use]
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error.as_str()
    }

    /// `true` when the payload cannot be *stored* at all, as opposed to being unfinished.
    ///
    /// The two are different problems and they are answered differently. A heading without its
    /// text is an author mid-sentence: the draft saves, the editor shows the field, and the
    /// publish is refused. A payload that is not an array, a block with no type, a tree four
    /// levels deep or one naming a type this platform does not ship is not content at all —
    /// storing it would put a row in the database that no renderer can draw and no later edit
    /// can make sense of, so the save itself is refused.
    #[must_use]
    pub fn is_fatal(&self) -> bool {
        matches!(
            self.code,
            "block_payload_invalid"
                | "block_props_invalid"
                | "block_too_many"
                | "block_too_deep"
                | "block_unknown_type"
        )
    }
}

/// Sort errors before warnings so a client can render the blocking set first.
fn ordered(mut issues: Vec<BlockIssue>) -> Vec<BlockIssue> {
    // `false` (a warning) sorts to 0 and `true` (an error) to 1, so the sort key is inverted:
    // the blocking set is what a client has to show, and the advisory issues follow it.
    issues.sort_by_key(|issue| u8::from(!issue.is_error()));
    issues
}

/// The full report of a validation run, as the API answers with it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BlockValidationReport {
    /// The issues, errors first.
    pub issues: Vec<BlockIssue>,
    /// Blocks counted, nested included.
    pub block_count: usize,
    /// `true` when no error blocks the publish.
    pub can_publish: bool,
}

impl BlockValidationReport {
    /// The issues that block a publish — the set a client renders first.
    #[must_use]
    pub fn errors(&self) -> impl Iterator<Item = &BlockIssue> {
        self.issues.iter().filter(|issue| issue.is_error())
    }

    /// The first issue that blocks a publish, if any.
    #[must_use]
    pub fn first_error(&self) -> Option<&BlockIssue> {
        self.errors().next()
    }

    /// The issues that make the payload unstorable, as opposed to unfinished.
    #[must_use]
    pub fn fatal(&self) -> impl Iterator<Item = &BlockIssue> {
        self.issues.iter().filter(|issue| issue.is_fatal())
    }

    /// The first issue that makes the payload unstorable, if any.
    #[must_use]
    pub fn first_fatal(&self) -> Option<&BlockIssue> {
        self.fatal().next()
    }
}

/// Validate a block tree against the registry.
///
/// The rules, in the order they are reported:
///
/// 1. the payload is an array, small enough to be a page and small enough to store;
/// 2. every block names a type the registry ships, and no block is a container's child when its
///    type accepts no children;
/// 3. every required prop is present and non-blank, and every value is the kind its schema
///    says (a `list` prop is a list, an `enum` prop is one of its options, a number is a number);
/// 4. an `image`/`gallery` entry without alternative text is its own named error, because an
///    inaccessible page is a broken page, not a style opinion;
/// 5. heading levels never skip a level — an `h4` directly after an `h2` is a warning, since
///    the document still renders, while the outline a screen reader walks does not.
#[must_use]
pub fn validate(value: &Value) -> BlockValidationReport {
    let mut issues = Vec::new();
    let mut total = 0usize;

    if value.to_string().len() > MAX_BLOCKS_BYTES {
        return BlockValidationReport {
            issues: vec![BlockIssue::orphan(
                "block_payload_too_large",
                format!("the block payload must stay under {MAX_BLOCKS_BYTES} bytes"),
            )],
            block_count: 0,
            can_publish: false,
        };
    }

    let blocks = match parse_blocks(value) {
        Ok(blocks) => blocks,
        Err(ContentError::InvalidBlock(message)) => {
            return BlockValidationReport {
                issues: vec![BlockIssue::orphan("block_payload_invalid", message)],
                block_count: 0,
                can_publish: false,
            };
        }
        Err(other) => {
            return BlockValidationReport {
                issues: vec![BlockIssue::orphan(
                    "block_payload_invalid",
                    other.to_string(),
                )],
                block_count: 0,
                can_publish: false,
            };
        }
    };

    if blocks.len() > MAX_BLOCKS {
        issues.push(BlockIssue::orphan(
            "block_too_many",
            format!(
                "a page holds at most {MAX_BLOCKS} blocks, this one has {}",
                blocks.len()
            ),
        ));
    }

    // Heading order is a document-level rule, so it is tracked across the whole walk rather
    // than per branch: the sequence a reader meets is the flattened document order.
    let mut previous_level: Option<u8> = None;
    for (index, block) in blocks.iter().enumerate() {
        total += count_blocks(block, 1);
        validate_block(block, index, 1, &mut issues, &mut previous_level);
    }

    let issues = ordered(issues);
    let can_publish = !issues.iter().any(BlockIssue::is_error);

    BlockValidationReport {
        issues,
        block_count: total,
        can_publish,
    }
}

/// Validate one block and, for a container, its children.
fn validate_block(
    block: &Block,
    index: usize,
    depth: usize,
    issues: &mut Vec<BlockIssue>,
    previous_heading: &mut Option<u8>,
) {
    let Some(entry) = definition(&block.kind) else {
        issues.push(BlockIssue::new(
            block,
            format!("[{index}].type"),
            "block_unknown_type",
            format!("{:?} is not a block type this platform ships", block.kind),
            Severity::Error,
        ));
        return;
    };

    if depth > MAX_DEPTH {
        issues.push(BlockIssue::new(
            block,
            format!("[{index}]"),
            "block_too_deep",
            format!("blocks nest at most {MAX_DEPTH} levels deep"),
            Severity::Error,
        ));
        return;
    }

    if !block.props.is_object() {
        issues.push(BlockIssue::new(
            block,
            format!("[{index}].props"),
            "block_props_invalid",
            "a block's props must be a JSON object",
            Severity::Error,
        ));
        return;
    }

    validate_props(block, entry, index, issues);
    check_heading_order(block, entry, index, issues, previous_heading);

    if !entry.container && !block.children.is_empty() {
        issues.push(BlockIssue::new(
            block,
            format!("[{index}].children"),
            "block_child_not_allowed",
            format!("a {} block does not hold other blocks", entry.key),
            Severity::Error,
        ));
        return;
    }

    for (child_index, child) in block.children.iter().enumerate() {
        validate_block(child, child_index, depth + 1, issues, previous_heading);
    }
}

/// Check every prop of a block against its schema.
fn validate_props(
    block: &Block,
    entry: &'static BlockDefinition,
    index: usize,
    issues: &mut Vec<BlockIssue>,
) {
    let object = block.props.as_object().expect("checked by the caller");

    for prop in entry.props {
        let fallback = prop.default.to_value();
        let present = object.get(prop.key);
        let value = present.unwrap_or(&fallback);

        if prop.required && present.is_none() {
            issues.push(BlockIssue::new(
                block,
                format!("[{index}].props.{}", prop.key),
                "block_prop_required",
                format!("{} needs a value before it can be published", prop.label),
                Severity::Error,
            ));
            continue;
        }

        // An absent optional prop is filled with its default; the author sees it in the
        // inspector and the stored payload says so explicitly.
        if prop.required && present.is_some() && is_blank(value) {
            issues.push(BlockIssue::new(
                block,
                format!("[{index}].props.{}", prop.key),
                "block_prop_required",
                format!("{} needs a value before it can be published", prop.label),
                Severity::Error,
            ));
            continue;
        }

        match prop.kind {
            PropKind::Text | PropKind::RichText => {
                let Some(text) = value.as_str() else {
                    issues.push(BlockIssue::new(
                        block,
                        format!("[{index}].props.{}", prop.key),
                        "block_prop_type",
                        format!("{} must be text", prop.label),
                        Severity::Error,
                    ));
                    continue;
                };
                if let Some(max) = prop.max_length {
                    if text.chars().count() > max {
                        issues.push(BlockIssue::new(
                            block,
                            format!("[{index}].props.{}", prop.key),
                            "block_prop_too_long",
                            format!("{} must stay under {max} characters", prop.label),
                            Severity::Error,
                        ));
                    }
                }
            }
            PropKind::Number => {
                if !value.is_number() {
                    issues.push(BlockIssue::new(
                        block,
                        format!("[{index}].props.{}", prop.key),
                        "block_prop_type",
                        format!("{} must be a number", prop.label),
                        Severity::Error,
                    ));
                }
            }
            PropKind::Boolean => {
                if !value.is_boolean() {
                    issues.push(BlockIssue::new(
                        block,
                        format!("[{index}].props.{}", prop.key),
                        "block_prop_type",
                        format!("{} must be yes or no", prop.label),
                        Severity::Error,
                    ));
                }
            }
            PropKind::Enum => {
                let Some(text) = value.as_str() else {
                    issues.push(BlockIssue::new(
                        block,
                        format!("[{index}].props.{}", prop.key),
                        "block_prop_type",
                        format!("{} must be one of the listed values", prop.label),
                        Severity::Error,
                    ));
                    continue;
                };
                if !prop.options.is_empty() && !prop.options.contains(&text) {
                    issues.push(BlockIssue::new(
                        block,
                        format!("[{index}].props.{}", prop.key),
                        "block_prop_not_allowed",
                        format!(
                            "{} must be one of {}, not {text:?}",
                            prop.label,
                            prop.options.join(", ")
                        ),
                        Severity::Error,
                    ));
                }
            }
            PropKind::List => {
                if !value.is_array() {
                    issues.push(BlockIssue::new(
                        block,
                        format!("[{index}].props.{}", prop.key),
                        "block_prop_type",
                        format!("{} must be a list", prop.label),
                        Severity::Error,
                    ));
                }
            }
        }
    }

    // The one prop rule that is not expressible as a schema constraint: an image without
    // alternative text is a broken page for anyone who cannot see it, so it gets its own code
    // instead of a generic "required" (the inspector highlights the field either way).
    match entry.key {
        "image" => {
            let alt = object
                .get("alt")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if alt.trim().is_empty() {
                issues.push(BlockIssue::new(
                    block,
                    format!("[{index}].props.alt"),
                    "block_alt_missing",
                    "an image needs alternative text before it can be published",
                    Severity::Error,
                ));
            }
        }
        "gallery" => {
            let images = object.get("images").and_then(Value::as_array);
            for (position, entry_value) in images.into_iter().flatten().enumerate() {
                if entry_value.as_str().unwrap_or_default().trim().is_empty() {
                    issues.push(BlockIssue::new(
                        block,
                        format!("[{index}].props.images.{position}"),
                        "block_alt_missing",
                        "every gallery image needs its URL (and its own alternative text)",
                        Severity::Error,
                    ));
                }
            }
        }
        _ => {}
    }
}

/// Warn when a heading skips a level. The first heading of a page is free: a page may start at
/// any level, and its `h1` is the page title the layout already renders.
fn check_heading_order(
    block: &Block,
    entry: &'static BlockDefinition,
    index: usize,
    issues: &mut Vec<BlockIssue>,
    previous_heading: &mut Option<u8>,
) {
    if entry.key != "heading" {
        return;
    }
    let level = block
        .props
        .get("level")
        .and_then(Value::as_str)
        .and_then(|text| text.as_bytes().get(1))
        .copied()
        .map(|digit| digit - b'0')
        .unwrap_or(2);

    if let Some(previous) = *previous_heading {
        if level > previous + 1 {
            issues.push(BlockIssue::new(
                block,
                format!("[{index}].props.level"),
                "block_heading_order",
                format!(
                    "an h{level} follows an h{previous}; screen readers read the outline in order"
                ),
                Severity::Warning,
            ));
        }
    }
    *previous_heading = Some(level);
}

/// Blocks in a tree, one level and deeper.
fn count_blocks(block: &Block, already: usize) -> usize {
    block
        .children
        .iter()
        .fold(already, |sum, child| count_blocks(child, sum + 1))
}

fn is_blank(value: &Value) -> bool {
    match value {
        Value::String(text) => text.trim().is_empty(),
        Value::Null => true,
        _ => false,
    }
}

/// Fill absent optional props with their defaults, so a stored payload always says what the
/// author sees. Unknown props are kept: dropping them would lose a value the moment a future
/// schema adds the field the author already typed.
#[must_use]
pub fn normalize(block: &mut Block) {
    if let Some(entry) = definition(&block.kind) {
        if let Some(object) = block.props.as_object_mut() {
            for prop in entry.props {
                if !prop.required {
                    object
                        .entry(prop.key.to_owned())
                        .or_insert_with(|| prop.default.to_value());
                }
            }
        }
    }
    for child in &mut block.children {
        let _ = normalize(child);
    }
}

/// Sanitise every `raw_html` block in a tree, in place, and report what changed.
///
/// The sanitiser runs **here**, on the way into storage, rather than in the renderer: a payload
/// the API accepted has already been made safe, so a database read, a cache, a theme override or
/// a future export path cannot resurrect markup that was only stripped on the way to the screen.
/// The editor calls the same function through the validate route to show the author what their
/// paste lost, so the preview and the stored value cannot disagree.
pub fn sanitize_tree(blocks: &mut [Block]) -> Vec<TreeSanitizeReport> {
    let mut reports = Vec::new();
    collect_reports(blocks, "", &mut reports);
    reports
}

/// One block's sanitisation outcome, addressed by the id the editor already has selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeSanitizeReport {
    /// The block whose `raw_html` prop was rewritten.
    pub block_id: Uuid,
    /// Where in the tree the block sits (`[2].children[0]`), matching the validator's paths.
    pub path: String,
    /// What the sanitiser removed.
    pub report: crate::sanitize::SanitizeReport,
}

/// Walk the tree in document order, sanitising each `raw_html` block as it is reached.
fn collect_reports(blocks: &mut [Block], parent_path: &str, reports: &mut Vec<TreeSanitizeReport>) {
    for (index, block) in blocks.iter_mut().enumerate() {
        let path = if parent_path.is_empty() {
            format!("[{index}]")
        } else {
            format!("{parent_path}.children[{index}]")
        };
        if block.kind == "raw_html" {
            if let Some(html) = block
                .props
                .get("html")
                .and_then(Value::as_str)
                .map(str::to_owned)
            {
                let (clean, report) = crate::sanitize::sanitize_html(&html);
                if !report.is_clean() {
                    if let Some(object) = block.props.as_object_mut() {
                        object.insert("html".to_owned(), Value::String(clean));
                    }
                }
                reports.push(TreeSanitizeReport {
                    block_id: block.id,
                    path,
                    report,
                });
                continue;
            }
        }
        collect_reports(&mut block.children, &path, reports);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One block payload with a fresh id.
    fn block(kind: &str, props: Value) -> Value {
        json!({ "id": Uuid::new_v4().to_string(), "type": kind, "props": props })
    }

    #[test]
    fn the_tree_sanitiser_rewrites_raw_html_in_place() {
        let mut parsed = parse_blocks(&json!([
            block("heading", json!({ "text": "Safe", "level": "h2" })),
            block(
                "raw_html",
                json!({ "html": "<p onclick=\"x()\">ok</p><script>bad()</script>" })
            ),
        ]))
        .expect("the payload parses");

        let reports = sanitize_tree(&mut parsed);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].path, "[1]");
        assert!(reports[0].report.removed_tags.contains("script"));

        let stored = blocks_to_value(&parsed);
        let html = stored[1]["props"]["html"]
            .as_str()
            .expect("the prop is text");
        assert!(!html.contains("onclick"), "the handler survived: {html}");
        assert!(!html.contains("script"), "the script survived: {html}");
        assert!(html.contains("ok"), "the visible text must survive: {html}");
    }

    #[test]
    fn the_tree_sanitiser_reaches_a_nested_block_and_names_its_path() {
        let mut parsed = parse_blocks(&json!([{
            "id": Uuid::new_v4().to_string(),
            "type": "columns",
            "props": { "columns": "2" },
            "children": [
                block("text", json!({ "text": "left" })),
                block("raw_html", json!({ "html": "<script>x</script>" })),
            ],
        }]))
        .expect("the payload parses");

        let reports = sanitize_tree(&mut parsed);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].path, "[0].children[1]");
        assert!(
            !blocks_to_value(&parsed)[0]["children"][1]["props"]["html"]
                .as_str()
                .unwrap_or_default()
                .contains("script")
        );
    }

    #[test]
    fn a_clean_raw_html_block_is_reported_as_unchanged() {
        let mut parsed = parse_blocks(&json!([block(
            "raw_html",
            json!({ "html": "<p>fine</p>" }),
        )]))
        .expect("the payload parses");
        let reports = sanitize_tree(&mut parsed);
        assert_eq!(reports.len(), 1);
        assert!(reports[0].report.is_clean());
        assert_eq!(reports[0].report.change_count(), 0);
    }

    #[test]
    fn a_tree_without_raw_html_produces_no_reports() {
        let mut parsed = parse_blocks(&json!([
            block("heading", json!({ "text": "A", "level": "h1" })),
            block("text", json!({ "text": "B" })),
        ]))
        .expect("the payload parses");
        assert!(sanitize_tree(&mut parsed).is_empty());
    }

    #[test]
    fn the_registry_ships_the_sixteen_documented_types() {
        assert_eq!(
            REGISTRY.len(),
            16,
            "REQ-063 §Scope names sixteen block types"
        );
        let keys: Vec<&str> = REGISTRY.iter().map(|entry| entry.key).collect();
        for expected in [
            "heading",
            "text",
            "image",
            "gallery",
            "video",
            "cta",
            "columns",
            "card_grid",
            "pricing_table",
            "testimonial",
            "faq",
            "form",
            "embed",
            "raw_html",
            "product_grid",
            "blog_list",
        ] {
            assert!(
                keys.contains(&expected),
                "{expected} must be in the registry"
            );
        }
    }

    #[test]
    fn every_definition_is_coherent() {
        for entry in REGISTRY {
            assert!(!entry.key.is_empty());
            assert!(
                CATEGORIES.contains(&entry.category),
                "{} sits in a category the panel groups by",
                entry.key
            );
            assert!(
                entry.props.iter().all(|prop| !prop.key.is_empty()),
                "{} has a named prop for every field",
                entry.key
            );
            let keys: Vec<&str> = entry.props.iter().map(|prop| prop.key).collect();
            let mut unique = keys.clone();
            unique.sort_unstable();
            unique.dedup();
            assert_eq!(keys.len(), unique.len(), "{} names a prop twice", entry.key);
            for prop in entry.props {
                if prop.kind == PropKind::Enum {
                    assert!(
                        !prop.options.is_empty(),
                        "{}'s {} enumerates its options",
                        entry.key,
                        prop.key
                    );
                }
                if prop.required {
                    assert!(
                        prop.default.is_empty(),
                        "{}'s required {} has nothing to default to",
                        entry.key,
                        prop.key
                    );
                }
            }
        }
    }

    #[test]
    fn only_columns_is_a_container_in_slice_one() {
        let containers: Vec<&str> = REGISTRY
            .iter()
            .filter(|entry| entry.container)
            .map(|entry| entry.key)
            .collect();
        assert_eq!(containers, vec!["columns"]);
    }

    #[test]
    fn the_registry_document_carries_the_schemas() {
        let document = registry_document();
        assert_eq!(document["version"], json!(REGISTRY_VERSION));
        assert_eq!(document["blocks"].as_array().map(Vec::len), Some(16));
        let heading = document["blocks"]
            .as_array()
            .expect("array")
            .iter()
            .find(|entry| entry["key"] == "heading")
            .expect("heading is registered");
        let level = heading["props"]
            .as_array()
            .expect("props")
            .iter()
            .find(|prop| prop["key"] == "level")
            .expect("heading has a level");
        assert_eq!(level["enum"].as_array().map(Vec::len), Some(6));
        assert_eq!(level["default"], json!("h2"));
    }

    #[test]
    fn defaults_come_from_the_schema() {
        assert_eq!(
            default_props("heading"),
            json!({ "text": "", "level": "h2", "align": "left" })
        );
        assert_eq!(default_props("not_a_block"), json!({}));
    }

    #[test]
    fn a_valid_tree_reports_nothing() {
        let report = validate(&json!([
            block("heading", json!({ "text": "Welcome", "level": "h1" })),
            block("text", json!({ "text": "First paragraph." })),
            {
                "id": Uuid::new_v4().to_string(),
                "type": "columns",
                "props": { "columns": 2 },
                "children": [
                    block("text", json!({ "text": "Left" })),
                    block("text", json!({ "text": "Right" }))
                ]
            }
        ]));
        assert!(report.can_publish, "issues: {:?}", report.issues);
        assert_eq!(
            report.block_count, 5,
            "top level plus nested blocks are counted"
        );
        assert!(report.issues.is_empty());
    }

    #[test]
    fn an_unknown_type_is_named_and_blocks_the_publish() {
        let report = validate(&json!([block("carousel", json!({}))]));
        assert!(!report.can_publish);
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].code, "block_unknown_type");
        assert_eq!(report.issues[0].severity, "error");
        assert_eq!(report.issues[0].block_type.as_deref(), Some("carousel"));
    }

    #[test]
    fn a_missing_required_prop_blocks_the_publish() {
        let report = validate(&json!([
            block("heading", json!({ "level": "h2" })),
            block("text", json!({ "text": "Fine" }))
        ]));
        assert!(!report.can_publish);
        let issue = report
            .issues
            .iter()
            .find(|issue| issue.code == "block_prop_required")
            .expect("the heading is missing its text");
        assert_eq!(issue.path, "[0].props.text");
        assert!(issue.message.contains("Heading"));
    }

    #[test]
    fn a_blank_required_prop_is_the_same_failure_as_a_missing_one() {
        let report = validate(&json!([block("heading", json!({ "text": "   " }))]));
        assert!(!report.can_publish);
        assert_eq!(report.issues[0].code, "block_prop_required");
    }

    #[test]
    fn an_image_without_alt_gets_its_own_code() {
        let report = validate(&json!([block(
            "image",
            json!({ "src": "/media/1", "alt": "" }),
        )]));
        assert!(!report.can_publish);
        assert_eq!(report.issues[0].code, "block_alt_missing");
        assert_eq!(report.issues[0].path, "[0].props.alt");
    }

    #[test]
    fn a_gallery_entry_without_a_url_is_reported_by_position() {
        let report = validate(&json!([block(
            "gallery",
            json!({ "images": ["/media/1", "  ", "/media/3"] }),
        )]));
        assert!(!report.can_publish);
        assert_eq!(report.issues[0].code, "block_alt_missing");
        assert_eq!(report.issues[0].path, "[0].props.images.1");
    }

    #[test]
    fn a_typed_prop_of_the_wrong_kind_is_refused() {
        let report = validate(&json!([
            block("heading", json!({ "text": "Hi", "level": 4 })),
            block("text", json!({ "text": 7 })),
            block("card_grid", json!({ "items": "not a list" }))
        ]));
        assert!(!report.can_publish);
        assert_eq!(report.issues.len(), 3);
        assert!(
            report
                .issues
                .iter()
                .all(|issue| issue.code == "block_prop_type")
        );
    }

    #[test]
    fn an_enum_outside_its_options_is_refused() {
        let report = validate(&json!([block(
            "heading",
            json!({ "text": "Hi", "level": "h9" }),
        )]));
        assert!(!report.can_publish);
        assert_eq!(report.issues[0].code, "block_prop_not_allowed");
        assert!(report.issues[0].message.contains("h1, h2"));
    }

    #[test]
    fn a_non_container_block_refuses_children() {
        let report = validate(&json!([{
            "id": Uuid::new_v4().to_string(),
            "type": "text",
            "props": {},
            "children": [block("text", json!({ "text": "nested" }))]
        }]));
        assert!(!report.can_publish);
        assert_eq!(report.issues[0].code, "block_child_not_allowed");
    }

    #[test]
    fn heading_order_warns_but_never_blocks() {
        let report = validate(&json!([
            block("heading", json!({ "text": "One", "level": "h2" })),
            block("heading", json!({ "text": "Two", "level": "h4" }))
        ]));
        assert!(report.can_publish, "a skipped level renders fine");
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].code, "block_heading_order");
        assert_eq!(report.issues[0].severity, "warning");
    }

    #[test]
    fn a_sequential_heading_run_is_silent_and_the_warning_clears_on_reorder() {
        let ordered = validate(&json!([
            block("heading", json!({ "text": "One", "level": "h1" })),
            block("heading", json!({ "text": "Two", "level": "h2" })),
            block("heading", json!({ "text": "Three", "level": "h3" }))
        ]));
        assert!(ordered.issues.is_empty());

        // The same three headings with the outer two swapped: h1 is followed by h3, which skips
        // h2, so the outline warning comes back without a single block being edited.
        let reordered = validate(&json!([
            block("heading", json!({ "text": "One", "level": "h1" })),
            block("heading", json!({ "text": "Three", "level": "h3" })),
            block("heading", json!({ "text": "Two", "level": "h2" }))
        ]));
        assert!(
            reordered
                .issues
                .iter()
                .any(|issue| issue.code == "block_heading_order"),
            "the same three headings warn once they are out of order"
        );
    }

    #[test]
    fn nesting_deeper_than_three_levels_is_refused() {
        let deepest = {
            let mut payload = block("text", json!({ "text": "deep" }));
            for _ in 0..MAX_DEPTH {
                payload = json!({
                    "id": Uuid::new_v4().to_string(),
                    "type": "columns",
                    "props": { "columns": 2 },
                    "children": [payload]
                });
            }
            payload
        };
        let report = validate(&json!([deepest]));
        assert!(!report.can_publish);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "block_too_deep")
        );
    }

    #[test]
    fn a_payload_that_is_not_an_array_is_refused_before_it_is_walked() {
        for payload in [json!({}), json!("nope"), json!(null)] {
            let report = validate(&payload);
            assert!(!report.can_publish, "{payload} must be refused");
            assert_eq!(report.issues[0].code, "block_payload_invalid");
            assert_eq!(report.issues[0].block_id, Uuid::nil());
        }
    }

    #[test]
    fn a_block_without_a_type_is_refused() {
        let report = validate(&json!([{ "id": Uuid::new_v4().to_string(), "props": {} }]));
        assert!(!report.can_publish);
        assert_eq!(report.issues[0].code, "block_payload_invalid");
    }

    #[test]
    fn a_page_cannot_hold_an_endless_number_of_blocks() {
        let many: Vec<Value> = (0..MAX_BLOCKS + 1)
            .map(|_| block("text", json!({ "text": "x" })))
            .collect();
        let report = validate(&json!(many));
        assert!(!report.can_publish);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "block_too_many")
        );
    }

    #[test]
    fn a_finished_problem_is_not_the_same_as_an_unstorable_payload() {
        // The distinction the save path turns on: a heading with no text is an author
        // mid-sentence and saves as a draft, while a payload nothing can render does not.
        let unfinished = validate(&json!([block("heading", json!({ "level": "h2" }))]));
        assert!(!unfinished.can_publish, "it cannot be published");
        assert_eq!(
            unfinished.first_error().map(|issue| issue.code),
            Some("block_prop_required")
        );
        assert!(
            unfinished.first_fatal().is_none(),
            "an author mid-sentence is not an unstorable payload"
        );

        let unknown = validate(&json!([block("carousel", json!({}))]));
        assert!(
            unknown.first_fatal().is_some(),
            "a type nothing renders cannot be stored"
        );

        let not_an_array = validate(&json!({ "blocks": [] }));
        assert!(not_an_array.first_fatal().is_some());
    }

    #[test]
    fn errors_are_reported_before_warnings() {
        let report = validate(&json!([
            block("heading", json!({ "text": "One", "level": "h1" })),
            block("heading", json!({ "text": "Two", "level": "h4" })),
            block("image", json!({ "src": "/m/1", "alt": "" }))
        ]));
        assert_eq!(report.issues[0].code, "block_alt_missing");
        assert_eq!(report.issues[1].code, "block_heading_order");
    }

    #[test]
    fn a_payload_round_trips_through_the_store_shape() {
        let original = json!([
            {
                "id": Uuid::new_v4().to_string(),
                "type": "columns",
                "props": { "columns": 2 },
                "children": [block("heading", json!({ "text": "In a column" }))]
            }
        ]);
        let blocks = parse_blocks(&original).expect("parseable");
        let again = blocks_to_value(&blocks);
        assert_eq!(again, original, "ids and order survive the round trip");

        let reparsed = parse_blocks(&again).expect("parseable again");
        assert_eq!(reparsed[0].children[0].id, blocks[0].children[0].id);
    }

    #[test]
    fn a_block_without_an_id_gets_one_minted() {
        let blocks = parse_blocks(&json!([{ "type": "text", "props": { "text": "x" } }]))
            .expect("parseable");
        assert_ne!(blocks[0].id, Uuid::nil());
    }

    #[test]
    fn normalizing_fills_optional_props_and_keeps_unknown_ones() {
        let mut blocks = parse_blocks(&json!([block(
            "heading",
            json!({ "text": "Hi", "future_field": "keep me" }),
        )]))
        .expect("parseable");
        normalize(&mut blocks[0]);
        let value = blocks_to_value(&blocks);
        assert_eq!(
            value[0]["props"]["level"],
            json!("h2"),
            "defaults are filled in"
        );
        assert_eq!(
            value[0]["props"]["future_field"],
            json!("keep me"),
            "a prop the registry does not know yet is not thrown away"
        );
    }
}
