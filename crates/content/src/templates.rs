//! The page templates that ship with the platform (REQ-063, slice 3).
//!
//! These live in **code**, not in this migration, for two reasons. A template is sample content —
//! it is the platform's answer to "what does a landing page look like here" — and sample content
//! belongs where it can be reviewed and tested in the same commit as the renderer that draws it.
//! And a template an operator hand-edited is a page nobody can reproduce, so the system set is
//! `is_system` and read-only in the gallery: it is a starting point, not a workspace.
//!
//! The blocks use the registry's own keys and prop names (`crates/content/src/blocks.rs`), and
//! `tests` below assert that, so a registry prop renamed without touching this file fails the
//! build rather than shipping a gallery full of blocks with a missing alt text.
//!
//! Copy is deliberately English and product-neutral: a template is a *structure* an author fills
//! with their own words, and Turkish example copy would be a sentence the author has to delete.

use serde_json::{Value, json};
use uuid::Uuid;

/// The key of the landing template.
pub const LANDING: &str = "landing";
/// The key of the about template.
pub const ABOUT: &str = "about";
/// The key of the pricing template.
pub const PRICING: &str = "pricing";
/// The key of the blog post template.
pub const BLOG_POST: &str = "blog-post";
/// The key of the contact template.
pub const CONTACT: &str = "contact";

/// One template the platform ships.
///
/// `PartialEq`/`Eq` are deliberately **not** derived: the struct carries a `fn() -> Value`
/// field, and comparing function addresses is not a meaningful equality — two entries can point
/// at the same generator or two generators can be merged into one address, so `==` on this type
/// would answer a question nobody asked with a coin flip. Templates are looked up by key.
#[derive(Debug, Clone, Copy)]
pub struct SystemTemplate {
    /// Stable key; also the seed's upsert key.
    pub key: &'static str,
    /// Name the gallery card shows.
    pub name: &'static str,
    /// Content type a page created from it gets.
    pub page_type: &'static str,
    /// One line describing what the template is for.
    pub description: &'static str,
    /// One line shown on the card as the template's outline.
    pub outline: &'static str,
    /// The page's blocks.
    pub blocks: fn() -> Value,
}

/// Every system template, in gallery order.
///
/// The order is the one an author browsing an empty site needs: a landing page first, then the
/// three "about us" pages every site has, then the one that has its own content type.
pub const SYSTEM_TEMPLATES: &[SystemTemplate] = &[
    SystemTemplate {
        key: LANDING,
        name: "Landing page",
        page_type: "page",
        description: "Hero, a value proposition, a feature grid and a closing call to action.",
        outline: "Hero · Value · Features · Call to action",
        blocks: landing_blocks,
    },
    SystemTemplate {
        key: ABOUT,
        name: "About",
        page_type: "page",
        description: "The story of the company, its values and the team behind it.",
        outline: "Intro · Story · Values · Team",
        blocks: about_blocks,
    },
    SystemTemplate {
        key: PRICING,
        name: "Pricing",
        page_type: "page",
        description: "A pricing table with the plans side by side and what each one includes.",
        outline: "Heading · Plans · Frequently asked",
        blocks: pricing_blocks,
    },
    SystemTemplate {
        key: BLOG_POST,
        name: "Blog post",
        page_type: "blog_post",
        description: "A long-form article layout with an author line and a related-posts rail.",
        outline: "Title · Body · Author · Related",
        blocks: blog_post_blocks,
    },
    SystemTemplate {
        key: CONTACT,
        name: "Contact",
        page_type: "page",
        description: "A contact heading, the details and a form block to collect enquiries.",
        outline: "Heading · Details · Form",
        blocks: contact_blocks,
    },
];

/// The system template with this key, when the platform ships one.
#[must_use]
pub fn find(key: &str) -> Option<&'static SystemTemplate> {
    SYSTEM_TEMPLATES.iter().find(|entry| entry.key == key)
}

fn heading(text: &str, level: u8) -> Value {
    json!({ "id": Uuid::new_v4(), "type": "heading", "props": { "text": text, "level": format!("h{level}") } })
}

fn text(value: &str) -> Value {
    json!({ "id": Uuid::new_v4(), "type": "text", "props": { "text": value } })
}

fn cta(headline: &str, body: &str, label: &str, url: &str) -> Value {
    json!({
        "id": Uuid::new_v4(),
        "type": "cta",
        "props": {
            "headline": headline,
            "body": body,
            "label": label,
            "url": url,
            "align": "center"
        }
    })
}

fn card_grid(items: Value) -> Value {
    json!({ "id": Uuid::new_v4(), "type": "card_grid", "props": { "items": items } })
}

fn column(children: Value) -> Value {
    json!({ "id": Uuid::new_v4(), "type": "column", "props": { "align": "left" }, "children": children })
}

fn columns(count: usize, children: Value) -> Value {
    json!({
        "id": Uuid::new_v4(),
        "type": "columns",
        "props": { "columns": count },
        "children": children
    })
}

fn cards() -> Value {
    json!([
        {
            "title": "Fast by default",
            "body": "Every screen loads the data it needs and nothing it does not.",
            "media_id": null
        },
        {
            "title": "Yours to shape",
            "body": "Blocks, themes and content types are all editable without a deploy.",
            "media_id": null
        },
        {
            "title": "Built to hand over",
            "body": "Roles, audit trails and backups are part of the product, not an add-on.",
            "media_id": null
        }
    ])
}

fn landing_blocks() -> Value {
    json!([
        heading("A headline that says what this is", 1),
        text("One sentence underneath it: who it is for and what changes for them."),
        cta(
            "Start with the free plan",
            "No card, no trial clock. Create an account and publish your first page.",
            "Get started",
            "/signup"
        ),
        heading("Why teams choose this", 2),
        card_grid(cards()),
        columns(2, json!([
            column(json!([
                heading("How it works", 2),
                text("Author in blocks, publish a revision, keep the history."),
            ])),
            column(json!([
                heading("What you get", 2),
                text("A CMS, a workflow engine and an API on one core."),
            ])),
        ])),
        cta("Ready when you are", "Create a page from this template and make it yours.", "Start now", "/signup"),
    ])
}

fn about_blocks() -> Value {
    json!([
        heading("The short version", 1),
        text("Replace this with a paragraph that says what the company does and for whom."),
        heading("How we got here", 2),
        text("A longer story: the problem that started it, and what changed since."),
        heading("What we value", 2),
        card_grid(cards()),
        heading("The people behind it", 2),
        text("Add a team block, or a card grid with one entry per person."),
    ])
}

fn pricing_blocks() -> Value {
    json!([
        heading("Simple, predictable pricing", 1),
        text("One sentence on how the plans differ and who each one is for."),
        {
            "id": Uuid::new_v4(),
            "type": "pricing_table",
            "props": {
                "caption": "All plans include the full API and every theme.",
                "rows": [
                    "Starter|29|Monthly|One site, 5 editors|[\"Block editor\",\"Ten pages\",\"Community support\"]",
                    "Team|79|Monthly|Five sites, unlimited editors|[\"Block editor\",\"Unlimited pages\",\"Roles and audit\",\"Priority support\"]",
                    "Scale|199|Monthly|Unlimited sites, SSO|[\"Everything in Team\",\"SSO and SCIM\",\"Backup centre\",\"Dedicated support\"]"
                ]
            }
        },
        heading("Frequently asked", 2),
        {
            "id": Uuid::new_v4(),
            "type": "faq",
            "props": {
                "items": [
                    "Can I change plan later?|Yes — upgrading takes effect immediately, and downgrading applies at the next renewal.",
                    "Is there a discount for yearly billing?|Yes, two months free on yearly billing.",
                    "What happens when I exceed my site limit?|Nothing breaks: the extra site is read-only until the plan is raised."
                ]
            }
        },
        cta("Still deciding?", "Start on Starter and move up when you need to.", "Start on Starter", "/signup"),
    ])
}

fn blog_post_blocks() -> Value {
    json!([
        heading("The title of the post", 1),
        text("The standfirst: one or two sentences a reader can decide from."),
        text("The body of the post. Paragraphs are separate text blocks, so a long read is built one block at a time and any of them can be moved."),
        {
            "id": Uuid::new_v4(),
            "type": "blog_list",
            "props": { "limit": 3, "heading": "More like this" }
        },
    ])
}

fn contact_blocks() -> Value {
    json!([
        heading("Talk to us", 1),
        text("Tell people how to reach you and what happens after they do."),
        columns(2, json!([
            column(json!([
                heading("Email", 2),
                text("hello@example.com"),
                heading("Phone", 2),
                text("+00 000 000 00"),
            ])),
            column(json!([
                heading("Office", 2),
                text("Street, city, country"),
                heading("Hours", 2),
                text("Monday to Friday, 09:00–18:00"),
            ])),
        ])),
        {
            "id": Uuid::new_v4(),
            "type": "form",
            "props": {
                "heading": "Send a message",
                "form_key": "contact",
                "submit_label": "Send"
            }
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::{blocks_to_value, parse_blocks, validate};

    #[test]
    fn every_system_template_is_a_valid_block_tree() {
        for template in SYSTEM_TEMPLATES {
            let payload = (template.blocks)();
            let parsed = parse_blocks(&payload)
                .unwrap_or_else(|error| panic!("{} is not readable: {error}", template.key));
            assert!(
                !parsed.is_empty(),
                "{} ships an empty page — a template with no blocks is a blank page with a name",
                template.key
            );
            let report = validate(&payload);
            for issue in report.issues.iter().filter(|issue| issue.is_fatal()) {
                panic!(
                    "{} carries a block the page save would refuse: [{}] {}",
                    template.key, issue.code, issue.message
                );
            }
        }
    }

    #[test]
    fn a_template_with_a_fresh_id_per_load_never_repeats_a_block_id() {
        let template = find(LANDING).expect("the landing template ships");
        let first = parse_blocks(&(template.blocks)()).expect("a readable tree");
        let second = parse_blocks(&(template.blocks)()).expect("a readable tree");
        let ids: Vec<Uuid> = first.iter().map(|block| block.id).collect();
        for block in &second {
            assert!(
                !ids.contains(&block.id),
                "two loads of {} share a block id, so two pages built from it collide",
                template.key
            );
        }
    }

    #[test]
    fn the_gallery_order_puts_the_landing_page_first() {
        let keys: Vec<&str> = SYSTEM_TEMPLATES.iter().map(|entry| entry.key).collect();
        assert_eq!(keys.first(), Some(&LANDING));
        assert_eq!(keys.len(), 5, "the REQ names five starting points");
        for wanted in [ABOUT, PRICING, BLOG_POST, CONTACT] {
            assert!(find(wanted).is_some(), "{wanted} must ship");
        }
    }

    #[test]
    fn keys_are_the_shape_the_store_accepts() {
        // The store's own validator, run against every shipped key: a template whose key the
        // database would refuse is a gallery card that cannot be created.
        for template in SYSTEM_TEMPLATES {
            crate::validation::validate_key(template.key, "key")
                .unwrap_or_else(|error| panic!("{} is not a usable key: {error}", template.key));
        }
    }

    #[test]
    fn a_blog_post_template_makes_a_blog_post_page() {
        let template = find(BLOG_POST).expect("the blog post template ships");
        assert_eq!(template.page_type, "blog_post");
        crate::validation::validate_page_type(template.page_type).expect("a real page type");
    }

    #[test]
    fn block_text_of_a_template_is_never_empty() {
        // The body a page created from a template carries is what search and SEO read; a
        // template whose words are all in non-text props would produce an unfindable page.
        let template = find(CONTACT).expect("the contact template ships");
        let blocks = parse_blocks(&(template.blocks)()).expect("a readable tree");
        let payload = blocks_to_value(&blocks);
        assert!(
            payload.to_string().contains("Talk to us"),
            "the template's own words are in its blocks"
        );
    }
}
