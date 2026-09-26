//! The command registry behind the palette (docs/requests/REQ-032).
//!
//! A command is **code, not configuration** — the documented decision of the request: the
//! palette's commands are compiled into the registry below, not stored in a table, so a command
//! cannot exist without the code that runs it. Every entry names the permission whose holder may
//! run it, and every navigation command names the screen it opens.
//!
//! Two promises the module keeps for its callers:
//!
//! * **The projection is the filter.** [`visible`] never returns a command the caller's own
//!   permission set does not cover, so the panel cannot render a command the API would refuse —
//!   the filter is applied here, not in the component.
//! * **An id is API surface.** Ids are stable (`nav.pages`); a retitled command keeps its old
//!   title as an alias, because automation that references an id must not break when a label is
//!   reworded.
//!
//! Icons are names the panel maps to its own icon set (lucide), never markup: the API answers
//! data, the panel decides how it looks.

/// What running a command does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
    /// Opens a screen. Running one writes nothing; `route` carries where it lands.
    Navigate,
    /// Does something through its owning service when it is run from the palette
    /// (`POST /api/v1/commands/{id}/run`). `route` names the screen that shows the record of the
    /// act, or is empty when the act has no screen (the palette's own history).
    Action,
}

impl CommandKind {
    /// The kind as the API spells it (`navigate`, `action`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Navigate => "navigate",
            Self::Action => "action",
        }
    }
}

/// One command the palette can offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    /// Stable id (`nav.pages`); this is the API surface automation references.
    pub id: &'static str,
    /// Title as the palette writes it (`Open pages`).
    pub title: &'static str,
    /// Group the palette lists it under (`Content`).
    pub group: &'static str,
    /// One line under the title saying what the command does.
    pub hint: &'static str,
    /// Icon name the panel maps to its own icon set (`file-text`).
    pub icon: &'static str,
    /// What running the command does: open a screen, or act through a service.
    pub kind: CommandKind,
    /// Permission whose holder may run it; `None` for commands every signed-in account holds.
    pub permission: Option<&'static str>,
    /// Where running it lands (`/pages`); may carry query parameters (`/pages?new=1`). An action
    /// either names the screen that reads its record back, or carries `""` when it has none.
    pub route: &'static str,
    /// `true` when the command asks the caller to confirm before it runs: it cannot be undone, or
    /// it rewrites a whole dataset. The API refuses such a command unless the caller confirms, so
    /// the question is a rule of the platform, not a decoration of one dialog.
    pub confirm: bool,
    /// Extra words a query may match on.
    pub keywords: &'static [&'static str],
    /// Former or colloquial titles; kept so old references keep working.
    pub aliases: &'static [&'static str],
    /// Route prefixes this command is a useful suggestion on (`/pages`).
    pub contexts: &'static [&'static str],
}

impl CommandSpec {
    /// What running this command does.
    #[must_use]
    pub fn kind(&self) -> CommandKind {
        self.kind
    }

    /// `true` when the command acts through a service rather than opening a screen.
    #[must_use]
    pub fn is_action(&self) -> bool {
        self.kind == CommandKind::Action
    }

    /// `true` when `needle` (already lower-cased and trimmed) appears in the title, the id, a
    /// keyword or an alias. The panel ranks its own matches; this is what "may match" means.
    #[must_use]
    pub fn matches(&self, needle: &str) -> bool {
        if needle.is_empty() {
            return true;
        }
        let haystacks = [self.title, self.id, self.hint];
        if haystacks
            .iter()
            .any(|value| value.to_lowercase().contains(needle))
        {
            return true;
        }
        self.keywords
            .iter()
            .chain(self.aliases.iter())
            .any(|value| value.to_lowercase().contains(needle))
    }
}

/// Every command the platform offers today, in palette order.
///
/// The list is deliberately the screens that exist: a command that opens nothing is worse than a
/// command that is not there. New modules contribute their own entries as they ship (the IAM
/// screens, CRM, workflows) and the palette picks them up without a front-end change.
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        id: "nav.overview",
        title: "Open overview",
        group: "Panel",
        hint: "The panel's home: what the installation looks like right now",
        icon: "layout-dashboard",
        kind: CommandKind::Navigate,
        permission: None,
        route: "/",
        confirm: false,
        keywords: &["home", "dashboard", "start", "landing"],
        aliases: &["home", "dashboard"],
        contexts: &[],
    },
    CommandSpec {
        id: "nav.search",
        title: "Search everything",
        group: "Panel",
        hint: "The full results screen with filters, selection and export",
        icon: "search",
        kind: CommandKind::Navigate,
        permission: Some("search.read"),
        route: "/search",
        confirm: false,
        keywords: &["find", "results", "query", "everything"],
        aliases: &["find", "results"],
        contexts: &["/pages", "/media", "/sites", "/ai", "/settings/search"],
    },
    CommandSpec {
        id: "nav.search-settings",
        title: "Open search settings",
        group: "Panel",
        hint: "Providers, states, ranking weights and reindexing",
        icon: "sliders-horizontal",
        kind: CommandKind::Navigate,
        permission: Some("search.manage"),
        route: "/settings/search",
        confirm: false,
        keywords: &[
            "index",
            "reindex",
            "weights",
            "ranking",
            "providers",
            "state",
        ],
        aliases: &["index settings", "search settings"],
        contexts: &["/search"],
    },
    CommandSpec {
        id: "nav.pages",
        title: "Open pages",
        group: "Content",
        hint: "The content of the selected site: drafts, revisions and publishing",
        icon: "file-text",
        kind: CommandKind::Navigate,
        permission: Some("content.pages.read"),
        route: "/pages",
        confirm: false,
        keywords: &["content", "articles", "posts", "documents"],
        aliases: &["pages", "content"],
        contexts: &["/media"],
    },
    CommandSpec {
        id: "nav.create-page",
        title: "Create a page",
        group: "Content",
        hint: "Opens the pages screen with the new-page form ready to type in",
        icon: "file-plus",
        kind: CommandKind::Navigate,
        permission: Some("content.pages.create"),
        route: "/pages?new=1",
        confirm: false,
        keywords: &["new page", "add page", "write", "article", "draft"],
        aliases: &["new page", "add page"],
        contexts: &["/pages", "/media"],
    },
    CommandSpec {
        id: "nav.media",
        title: "Open media",
        group: "Content",
        hint: "The file library of the selected site",
        icon: "images",
        kind: CommandKind::Navigate,
        permission: Some("media.read"),
        route: "/media",
        confirm: false,
        keywords: &["files", "images", "uploads", "library", "assets"],
        aliases: &["files", "upload files"],
        contexts: &["/pages"],
    },
    CommandSpec {
        id: "nav.sites",
        title: "Open sites",
        group: "Platform",
        hint: "The sites of the organization and the domains they answer on",
        icon: "globe",
        kind: CommandKind::Navigate,
        permission: Some("sites.read"),
        route: "/sites",
        confirm: false,
        keywords: &["domains", "website", "sites", "hosts"],
        aliases: &["domain", "website"],
        contexts: &[],
    },
    CommandSpec {
        id: "nav.ai",
        title: "Open the AI hub",
        group: "Platform",
        hint: "Providers, the model registry and the chat playground",
        icon: "sparkles",
        kind: CommandKind::Navigate,
        permission: Some("ai.providers.read"),
        route: "/ai",
        confirm: false,
        keywords: &["models", "providers", "assistant", "chat", "ai"],
        aliases: &["ai", "models"],
        contexts: &[],
    },
    CommandSpec {
        id: "act.reindex-search",
        title: "Rebuild the search index",
        group: "Search",
        hint: "Re-reads every provider's rows and rewrites the whole search index",
        icon: "refresh-cw",
        kind: CommandKind::Action,
        permission: Some("search.manage"),
        route: "/settings/search",
        confirm: true,
        keywords: &[
            "reindex", "rebuild", "index", "refresh", "search", "backfill",
        ],
        aliases: &["reindex", "rebuild the index", "reindex search"],
        contexts: &["/search"],
    },
    CommandSpec {
        id: "act.clear-recents",
        title: "Clear command history",
        group: "Panel",
        hint: "Forgets every search and command this account remembers in the palette",
        icon: "eraser",
        kind: CommandKind::Action,
        permission: None,
        // Nothing to open afterwards: the palette's own list is the surface that changes.
        route: "",
        confirm: true,
        keywords: &["clear", "history", "forget", "recents", "palette", "reset"],
        aliases: &["clear recents", "clear recent searches", "forget history"],
        contexts: &[],
    },
];

/// One command by its stable id.
#[must_use]
pub fn command(id: &str) -> Option<&'static CommandSpec> {
    COMMANDS.iter().find(|spec| spec.id == id)
}

/// One command by its id **when it is an action** — the only kind `POST
/// /api/v1/commands/{id}/run` executes.
///
/// A navigation command is deliberately not runnable: it opens a screen, and a caller that wants
/// that screen says so by opening it. Keeping the two apart here means the API's refusal
/// (`not_runnable`) cannot drift from the registry's own answer.
#[must_use]
pub fn runnable(id: &str) -> Option<&'static CommandSpec> {
    command(id).filter(|spec| spec.is_action())
}

/// Every command of the registry whose id appears in `ids`, in registry order.
#[must_use]
pub fn known(ids: &[&str]) -> Vec<&'static CommandSpec> {
    COMMANDS
        .iter()
        .filter(|spec| ids.contains(&spec.id))
        .collect()
}

/// The commands a caller holding `allows` may run, in registry order.
///
/// `allows` answers the permission question for one key — the caller's effective permission set
/// lives in `omnion-permissions`, which this crate deliberately does not depend on: the registry
/// asks, it does not resolve.
#[must_use]
pub fn visible(allows: &dyn Fn(&str) -> bool) -> Vec<&'static CommandSpec> {
    COMMANDS
        .iter()
        .filter(|spec| spec.permission.is_none_or(allows))
        .collect()
}

/// The route prefix a path is matched against (`/pages?new=1` → `/pages`, `` → `/`).
fn path_of(route: &str) -> &str {
    let path = route.split(['?', '#']).next().unwrap_or("");
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() { "/" } else { trimmed }
}

/// The commands worth suggesting on `route`: the visible ones whose own screen is somewhere else
/// and whose context list covers this path, best first, capped at [`SUGGEST_LIMIT`].
///
/// A command that opens the very URL the caller is already on is left out (`Open pages` on
/// `/pages`), because a suggestion that changes nothing is noise — while `Create a page`
/// (`/pages?new=1`) on `/pages` stays, since it lands on the form rather than the list.
#[must_use]
pub fn suggest(route: &str, allows: &dyn Fn(&str) -> bool) -> Vec<&'static CommandSpec> {
    let path = path_of(route);
    let current = route.trim_end_matches('/');
    let current = if current.is_empty() { "/" } else { current };

    let mut hits: Vec<&'static CommandSpec> = visible(allows)
        .into_iter()
        .filter(|spec| spec.route.trim_end_matches('/') != current)
        .filter(|spec| {
            spec.contexts
                .iter()
                .any(|context| path == *context || path.starts_with(&format!("{context}/")))
        })
        .collect();

    // A screen none of the entries names (a new route, a deep link) still gets a useful trio:
    // the box itself, then the two doors most panels live behind.
    if hits.is_empty() {
        hits = visible(allows)
            .into_iter()
            .filter(|spec| ["nav.search", "nav.pages", "nav.media"].contains(&spec.id))
            .collect();
    }

    hits.truncate(SUGGEST_LIMIT);
    hits
}

/// How many suggestions one route gets.
pub const SUGGEST_LIMIT: usize = 4;

#[cfg(test)]
mod tests {
    use super::*;

    /// A caller holding exactly `keys`.
    fn allows(keys: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |key: &str| keys.contains(&key)
    }

    #[test]
    fn ids_are_unique_and_stable_in_shape() {
        let mut ids: Vec<&str> = COMMANDS.iter().map(|spec| spec.id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "command ids must be unique");
        assert!(
            COMMANDS.iter().all(|spec| spec.id.split('.').count() == 2
                && (spec.kind == CommandKind::Action || spec.route.starts_with('/'))),
            "ids are `<area>.<action>`; a navigation command always lands on a route, an action \
             names the screen that reads its record back or carries none"
        );
        assert!(
            COMMANDS
                .iter()
                .filter(|spec| spec.route.is_empty())
                .all(|spec| spec.is_action()),
            "only an action may carry no route: it never opens a screen"
        );
    }

    #[test]
    fn only_an_action_can_be_run_and_an_action_says_whether_it_asks_first() {
        // The two kinds are told apart by the registry itself, so the API's refusal
        // (`not_runnable`) can never drift from what the projection says.
        assert!(runnable("nav.pages").is_none());
        assert!(runnable("nav.create-page").is_none());
        assert_eq!(runnable("act.nope"), None);

        let reindex = runnable("act.reindex-search").expect("the index rebuild is runnable");
        assert_eq!(reindex.kind(), CommandKind::Action);
        assert_eq!(reindex.permission, Some("search.manage"));
        assert!(reindex.confirm, "a whole-index rebuild asks before it runs");
        assert_eq!(reindex.route, "/settings/search");

        let clear = runnable("act.clear-recents").expect("clearing the history is runnable");
        assert!(
            clear.permission.is_none(),
            "the account's own history is every signed-in caller's to clear"
        );
        assert!(clear.confirm, "a clear cannot be undone, so it asks first");
        assert_eq!(clear.route, "");

        assert_eq!(CommandKind::Navigate.as_str(), "navigate");
        assert_eq!(CommandKind::Action.as_str(), "action");
    }

    #[test]
    fn every_action_command_matches_its_own_words() {
        let reindex = command("act.reindex-search").expect("registered");
        assert!(reindex.matches("reindex"));
        assert!(reindex.matches("rebuild"));
        assert!(reindex.matches("search"));
        assert!(!reindex.matches("invoice"));

        let clear = command("act.clear-recents").expect("registered");
        assert!(clear.matches("clear"));
        assert!(clear.matches("forget history"));
        assert!(!clear.matches("reindex"));
    }

    #[test]
    fn the_projection_never_hands_a_member_an_action_it_may_not_run() {
        let member = visible(&allows(&["search.read"]));
        assert!(
            !member.iter().any(|spec| spec.id == "act.reindex-search"),
            "search.manage is not part of a member's keys"
        );
        assert!(
            member.iter().any(|spec| spec.id == "act.clear-recents"),
            "clearing one's own history needs no key beyond being signed in"
        );

        let manager = visible(&allows(&["search.read", "search.manage"]));
        assert!(manager.iter().any(|spec| spec.id == "act.reindex-search"));
    }

    #[test]
    fn a_command_matches_its_title_keywords_and_aliases() {
        let page = command("nav.create-page").expect("the id is registered");
        assert!(page.matches(""));
        assert!(page.matches("create"));
        assert!(page.matches("draft"));
        assert!(page.matches("add page"));
        assert!(!page.matches("invoice"));
        assert_eq!(page.kind(), CommandKind::Navigate);
    }

    #[test]
    fn the_projection_never_returns_a_command_the_caller_lacks() {
        let member = visible(&allows(&["search.read"]));
        assert!(member.iter().any(|spec| spec.id == "nav.overview"));
        assert!(member.iter().any(|spec| spec.id == "nav.search"));
        assert!(
            !member.iter().any(|spec| spec.id == "nav.pages"),
            "an account without content.pages.read never sees the pages command"
        );
        assert!(
            !member.iter().any(|spec| spec.id == "nav.create-page"),
            "creating a page needs its own key, not the read one"
        );

        let creator = visible(&allows(&["content.pages.create"]));
        assert!(creator.iter().any(|spec| spec.id == "nav.create-page"));
        assert!(
            !creator.iter().any(|spec| spec.id == "nav.pages"),
            "holding create does not imply the read key"
        );

        assert_eq!(
            visible(&allows(&[])).len(),
            2,
            "only the panel home and clearing one's own palette history are unguarded"
        );
    }

    #[test]
    fn suggestions_follow_the_screen_and_leave_out_what_is_already_open() {
        let owner = allows(&[
            "search.read",
            "search.manage",
            "content.pages.read",
            "content.pages.create",
            "media.read",
            "sites.read",
            "ai.providers.read",
        ]);

        let on_pages = suggest("/pages", &owner);
        let ids: Vec<&str> = on_pages.iter().map(|spec| spec.id).collect();
        assert!(
            ids.contains(&"nav.create-page"),
            "the pages screen suggests its own create form, got {ids:?}"
        );
        assert!(
            !ids.contains(&"nav.pages"),
            "the screen the caller is on is never a suggestion"
        );
        assert!(on_pages.len() <= SUGGEST_LIMIT);

        // A deep link with a query still matches its own path; an unknown route falls back.
        assert!(!suggest("/pages?focus=123", &owner).is_empty());
        let stranger = suggest("/reports/2026", &owner);
        assert_eq!(
            stranger.first().map(|spec| spec.id),
            Some("nav.search"),
            "an unnamed screen falls back to the box itself"
        );
        assert!(stranger.len() <= SUGGEST_LIMIT);
    }

    #[test]
    fn suggestions_respect_the_callers_permissions() {
        let member = suggest("/pages", &allows(&["search.read"]));
        let ids: Vec<&str> = member.iter().map(|spec| spec.id).collect();
        assert!(ids.contains(&"nav.search"));
        assert!(!ids.contains(&"nav.create-page"));
        assert!(!ids.contains(&"nav.media"));
    }
}
