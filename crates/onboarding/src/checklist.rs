//! The getting-started checklist (REQ-050) the dashboard shows after the first run.
//!
//! Items are derived from the platform's own rows, so they complete by themselves: publishing a
//! page ticks "Publish a page" without anything having to remember that it happened. Each item
//! links to the screen that does the work — and the wizard screen is only linked while the item
//! is still open, so a finished setup never points anywhere the panel cannot go.

use omnion_content::pages;
use omnion_identity::sites;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::Result;

/// One line of the getting-started checklist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecklistItem {
    /// Stable key (`first_page`).
    pub key: &'static str,
    /// Short label.
    pub label: &'static str,
    /// One line explaining what the item is.
    pub description: &'static str,
    /// Panel screen that does the work.
    pub href: &'static str,
    /// `true` when the installation already satisfies the item.
    pub done: bool,
}

/// Build the checklist of an installation.
///
/// `site_id` is the first site (the one the wizard created), when there is one; `organization`
/// says whether a tenant exists yet.
pub async fn build(
    pool: &PgPool,
    site_id: Option<Uuid>,
    organization: bool,
) -> Result<Vec<ChecklistItem>> {
    let (pages_total, pages_published, domains) = match site_id {
        Some(site_id) => {
            let pages = pages::list_pages(pool, site_id, None).await?;
            let published = pages.iter().filter(|page| page.is_published()).count();
            let domains = sites::list_domains(pool, site_id).await?.len();
            (pages.len(), published, domains)
        }
        None => (0, 0, 0),
    };
    let accounts = omnion_identity::count_users(pool).await?;

    Ok(vec![
        ChecklistItem {
            key: "account",
            label: "Create the owner account",
            description: "The first account owns the installation.",
            href: "/setup",
            done: accounts > 0,
        },
        ChecklistItem {
            key: "organization",
            label: "Create your organization",
            description: "Sites, content and members live inside an organization.",
            href: "/setup",
            done: organization,
        },
        ChecklistItem {
            key: "site",
            label: "Create your first site",
            description: "A site is the property your visitors read.",
            href: "/setup",
            done: site_id.is_some(),
        },
        ChecklistItem {
            key: "first_page",
            label: "Create your first page",
            description: "Pages carry the content visitors read.",
            href: "/pages",
            done: pages_total > 0,
        },
        ChecklistItem {
            key: "first_publish",
            label: "Publish a page",
            description: "A page is public once it has a published revision.",
            href: "/pages",
            done: pages_published > 0,
        },
        ChecklistItem {
            key: "domain",
            label: "Connect a domain",
            description: "Bind the host visitors type to the site.",
            href: "/sites",
            done: domains > 0,
        },
    ])
}
