//! The intent grammar behind the palette's natural-language resolution (docs/requests/REQ-032,
//! slice 4).
//!
//! One promise: **a phrase becomes a structure, and the structure is checked against the
//! platform's own vocabulary** — the providers the index really has, the commands the registry
//! really offers — never against a guess. Everything here is pure (no database, no network), so
//! the grammar can be unit-tested without a stack and the same answer can be replayed for the
//! same words.
//!
//! The module answers three questions for its callers:
//!
//! * [`parse`] — what did the reader mean? A search over a domain (with the filters the index
//!   supports), a command from the registry, or nothing recognisable.
//! * [`Intent::preview`] — the same intent in plain words ("Tickets · assignee: Mehmet · last
//!   10 · newest first"), so the operator can correct it before anything runs.
//! * [`alternatives`] — what else the words could have meant, drawn from the commands and the
//!   providers that exist today and nothing else.
//!
//! Confidence is computed, never guessed: it rises with each part of the sentence the grammar
//! recognised (a verb, a domain, a filter) and with whether the domain maps to a provider the
//! index answers for. A caller that needs a decision uses the thresholds below rather than
//! inventing its own.
//!
//! What the grammar deliberately does **not** do: invent a domain the platform has no screen for
//! (the words stay a search), or name a command the registry does not hold. A model plugged in
//! front of it (the API's resolver) is normalised against exactly these tables, so a wrong answer
//! from a model degrades to this grammar rather than to a dead end.

use crate::commands::{self, CommandKind, CommandSpec};
use crate::providers::PROVIDERS;

/// At or above this confidence an interpretation is as good as a decision.
pub const HIGH_CONFIDENCE: f32 = 0.75;
/// At or above this confidence — and with a destination the platform really has — a caller may
/// run the interpretation. Below it the card offers alternatives instead: nothing executes on a
/// guess.
pub const RUN_CONFIDENCE: f32 = 0.6;
/// How many suggestions the "did you mean" list carries at most.
pub const MAX_ALTERNATIVES: usize = 3;
/// Longest query text the grammar reads (mirrors the palette's own cap).
pub const MAX_QUERY_CHARS: usize = 200;

/// What running an interpretation would do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentKind {
    /// A search: the words over the index, optionally narrowed to one domain.
    Search,
    /// A command from the registry, by its stable id.
    Command,
    /// Nothing recognisable: the words are still a search, but no interpretation is claimed.
    Unclear,
}

impl IntentKind {
    /// The kind as the API spells it (`search`, `command`, `unclear`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Command => "command",
            Self::Unclear => "unclear",
        }
    }
}

/// One filter the phrase carried, in the words the preview prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntentFilter {
    /// Stable key (`assignee`).
    pub key: &'static str,
    /// Label as the preview writes it (`assignee`).
    pub label: &'static str,
    /// What was filtered on, as the reader wrote it (`Mehmet`).
    pub value: String,
}

/// One phrase, read.
#[derive(Debug, Clone, PartialEq)]
pub struct Intent {
    /// What running it would do.
    pub kind: IntentKind,
    /// The domain word the reader used, as written (`Tickets`); `None` when the phrase named none.
    pub entity: Option<String>,
    /// The provider the domain maps to, when the index has one.
    pub provider: Option<&'static str>,
    /// The registry id a command interpretation named; dropped by the API when the caller's
    /// permissions do not cover it.
    pub command_id: Option<&'static str>,
    /// The words a search would carry (filters removed), as written.
    pub query: String,
    /// Filters the phrase carried, in the order they were recognised.
    pub filters: Vec<IntentFilter>,
    /// `newest`, `oldest` or `title`; `None` when the phrase asked for no order.
    pub sort: Option<&'static str>,
    /// "last 10" — how many results the phrase asked for.
    pub limit: Option<u32>,
    /// Computed confidence, `0.0`–`0.95`.
    pub confidence: f32,
}

impl Intent {
    /// One interpretation with nothing recognised in it.
    fn nothing(query: &str) -> Self {
        Self {
            kind: IntentKind::Unclear,
            entity: None,
            provider: None,
            command_id: None,
            query: query.to_owned(),
            filters: Vec::new(),
            sort: None,
            limit: None,
            confidence: 0.0,
        }
    }

    /// The command a command interpretation names.
    #[must_use]
    pub fn command(&self) -> Option<&'static CommandSpec> {
        self.command_id.and_then(commands::command)
    }

    /// `true` when the interpretation points somewhere the platform really has — and the caller
    /// may really open — and its confidence is high enough to act on.
    ///
    /// Three ways to answer `false`, and all three mean the same thing to the panel: a command the
    /// caller lacks the key for, a domain the index does not answer for (or the caller cannot
    /// read), and a shaky reading. Nothing executes on any of them.
    #[must_use]
    pub fn is_runnable(&self, allows: &dyn Fn(&str) -> bool) -> bool {
        if self.confidence < RUN_CONFIDENCE {
            return false;
        }
        match self.kind {
            IntentKind::Command => self
                .command()
                .is_some_and(|spec| spec.permission.is_none_or(allows)),
            IntentKind::Search => self
                .provider
                .and_then(crate::providers::provider)
                .is_some_and(|spec| allows(spec.permission)),
            IntentKind::Unclear => false,
        }
    }

    /// `true` when the interpretation names a command that acts through a service (rather than
    /// opening a screen) and therefore has to go through the run endpoint (and its confirmation).
    #[must_use]
    pub fn runs_through_a_service(&self) -> bool {
        self.command()
            .is_some_and(|spec| spec.kind() == CommandKind::Action)
    }

    /// The interpretation in plain words: "Tickets · assignee: Mehmet · last 10 · newest first".
    ///
    /// Built from the parts that were *recognised*, so the operator reads what the platform
    /// understood rather than a paraphrase of what they typed. An unreadable phrase says so
    /// instead of dressing the words up as an interpretation.
    #[must_use]
    pub fn preview(&self) -> String {
        if self.kind == IntentKind::Unclear {
            return format!("Search for “{}”", self.query);
        }
        if let Some(spec) = self.command() {
            let mut line = spec.title.to_owned();
            if self.runs_through_a_service() && spec.confirm {
                line.push_str(" · asks first");
            }
            return line;
        }

        let mut parts: Vec<String> = Vec::new();
        if let Some(entity) = &self.entity {
            parts.push(title_case(entity));
        }
        for filter in &self.filters {
            parts.push(format!("{}: {}", filter.label, filter.value));
        }
        if let Some(limit) = self.limit {
            parts.push(format!("last {limit}"));
        }
        if let Some(sort) = self.sort {
            parts.push(sort_label(sort).to_owned());
        }
        if parts.is_empty() {
            parts.push(format!("Search for “{}”", self.query));
        }
        parts.join(" · ")
    }

    /// Where the interpretation's own search goes, filters applied — the `Edit as search`
    /// destination and the destination a runnable search opens.
    ///
    /// The words travel in `q` (what the reader typed, without the filters the grammar consumed),
    /// the domain travels as the results screen's own `type` parameter, and the order as `sort` —
    /// so the screen lands with its chips already applied.
    #[must_use]
    pub fn search_url(&self) -> String {
        let mut url = String::from("/search?q=");
        let words = if self.query.trim().is_empty() {
            self.entity.clone().unwrap_or_default()
        } else {
            self.query.clone()
        };
        url.push_str(&encode_component(&words));
        if let Some(provider) = self.provider {
            url.push_str("&type=");
            url.push_str(provider);
        }
        if let Some(sort) = self.sort {
            url.push_str("&sort=");
            url.push_str(sort);
        }
        url
    }

    /// Where a command interpretation lands, when its command opens a screen.
    #[must_use]
    pub fn command_route(&self) -> Option<&'static str> {
        let spec = self.command()?;
        if spec.route.is_empty() {
            return None;
        }
        Some(spec.route)
    }

    /// Drop the command this interpretation names when `allows` does not cover it.
    ///
    /// The grammar is pure and knows nothing about accounts, so the permission filter belongs to
    /// the layer that does. A dropped command degrades to the search the words also describe, so
    /// a phrase reads as "the search" rather than as a refusal.
    pub fn keep_only_commands(&mut self, allows: &dyn Fn(&str) -> bool) {
        let Some(spec) = self.command() else {
            return;
        };
        if spec.permission.is_none_or(allows) {
            return;
        }
        self.command_id = None;
        if self.query.trim().is_empty() && self.entity.is_none() {
            self.kind = IntentKind::Unclear;
            self.confidence = self.confidence.min(0.4);
        } else {
            self.kind = IntentKind::Search;
            self.confidence = (self.confidence - 0.15).max(0.3);
        }
    }
}

/// One thing the words could have meant instead of what the grammar read.
#[derive(Debug, Clone, PartialEq)]
pub struct Alternative {
    /// The line the card prints ("Open pages", "Search everything for “tickets”").
    pub label: String,
    /// `search` or `command` — only screens: an alternative never runs an action by accident.
    pub kind: &'static str,
    /// Where activating it goes.
    pub route: String,
    /// Provider a search alternative narrows to.
    pub provider: Option<&'static str>,
    /// Registry id a command alternative names.
    pub command_id: Option<&'static str>,
    /// How well the words matched, `0.0`–`1.0`; the list is ordered by it.
    pub confidence: f32,
}

/// Words that say what the reader wants done rather than what they want.
const VERBS: &[&str] = &[
    "open", "show", "see", "view", "display", "list", "go", "goto", "launch", "find", "search",
    "jump", "browse",
];

/// Words that carry no meaning of their own.
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "my", "me", "mine", "our", "ours", "of", "for", "in", "on", "at", "to",
    "into", "from", "with", "and", "or", "all", "any", "please", "then", "that", "this", "these",
    "those", "about", "into", "onto", "up", "down", "over", "under", "is", "are", "was", "were",
    "give", "get", "take", "want", "need", "would", "could", "should", "can", "let", "us", "do",
    "does", "it", "its", "s", "t",
];

/// Domain words, mapped to the provider the index answers with.
///
/// The table is the vocabulary: a word outside it is never silently treated as a domain — it
/// stays a word of the search.
const DOMAINS: &[(&str, &str)] = &[
    ("page", "pages"),
    ("pages", "pages"),
    ("content", "pages"),
    ("article", "pages"),
    ("articles", "pages"),
    ("post", "pages"),
    ("posts", "pages"),
    ("document", "pages"),
    ("documents", "pages"),
    ("media", "media"),
    ("file", "media"),
    ("files", "media"),
    ("image", "media"),
    ("images", "media"),
    ("picture", "media"),
    ("pictures", "media"),
    ("photo", "media"),
    ("photos", "media"),
    ("upload", "media"),
    ("uploads", "media"),
    ("asset", "media"),
    ("assets", "media"),
    ("user", "users"),
    ("users", "users"),
    ("account", "users"),
    ("accounts", "users"),
    ("people", "users"),
    ("person", "users"),
    ("member", "users"),
    ("members", "users"),
    ("teammate", "users"),
    ("teammates", "users"),
    ("site", "sites"),
    ("sites", "sites"),
    ("domain", "sites"),
    ("domains", "sites"),
    ("website", "sites"),
    ("websites", "sites"),
    ("host", "sites"),
    ("hosts", "sites"),
    ("setting", "settings"),
    ("settings", "settings"),
    ("preference", "settings"),
    ("preferences", "settings"),
    ("config", "settings"),
    ("configuration", "settings"),
    ("log", "logs"),
    ("logs", "logs"),
    ("activity", "logs"),
    ("activities", "logs"),
    ("audit", "logs"),
    ("audits", "logs"),
    ("event", "logs"),
    ("events", "logs"),
    ("trail", "logs"),
    ("translation", "translations"),
    ("translations", "translations"),
    ("locale", "translations"),
    ("locales", "translations"),
    ("language", "translations"),
    ("languages", "translations"),
];

/// The provider one domain word names, when the index answers for it.
fn domain_of(word: &str) -> Option<&'static str> {
    let word = word.to_lowercase();
    DOMAINS
        .iter()
        .find(|(needle, _)| *needle == word)
        .map(|(_, provider)| *provider)
        .filter(|provider| crate::providers::provider(provider).is_some())
}

/// The provider order, so a phrase naming two domains reads in the index's own order.
fn provider_rank(key: &str) -> usize {
    PROVIDERS
        .iter()
        .position(|spec| spec.key == key)
        .unwrap_or(usize::MAX)
}

/// A sort order as the preview prints it.
fn sort_label(sort: &str) -> &'static str {
    match sort {
        "newest" => "newest first",
        "oldest" => "oldest first",
        "title" => "title order",
        _ => "best match",
    }
}

/// Capitalise one word the way a sentence starts ("tickets" → "Tickets").
fn title_case(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// `true` when the token is a number the phrase could be asking a count of (1–99).
fn small_number(token: &str) -> Option<u32> {
    if token.len() > 2 || !token.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let value: u32 = token.parse().ok()?;
    (1..=99).contains(&value).then_some(value)
}

/// How many words a query text holds.
fn content_len(text: &str) -> usize {
    text.split_whitespace().count()
}

/// Strip the punctuation a word may carry at its edges, keeping what is inside it.
fn clean(token: &str) -> &str {
    token.trim_matches(|c: char| {
        matches!(
            c,
            '"' | '\'' | '’' | '‘' | '“' | '”' | ',' | '.' | '?' | '!' | ':' | ';' | '(' | ')'
        )
    })
}

/// The name a possessive token carries ("Mehmet's" → "Mehmet").
fn possessive(token: &str) -> Option<&str> {
    let lowered = token.to_lowercase();
    for suffix in ["'s", "’s"] {
        if lowered.ends_with(suffix) {
            let end = token.len() - suffix.len();
            let name = clean(&token[..end]);
            if name.chars().count() >= 2 {
                return Some(name);
            }
        }
    }
    None
}

/// Read one phrase into an intent.
///
/// Deterministic by construction: the same words always produce the same structure, which is what
/// lets the module be tested without a model and replayed in a bug report.
#[must_use]
pub fn parse(text: &str) -> Intent {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Intent::nothing("");
    }
    let capped: String = trimmed.chars().take(MAX_QUERY_CHARS).collect();
    let tokens: Vec<&str> = capped.split_whitespace().collect();
    let mut used = vec![false; tokens.len()];
    let mut filters: Vec<IntentFilter> = Vec::new();

    // 1. The order first: a phrase that asks for the newest thing is not asking for a screen.
    let mut sort: Option<&'static str> = None;
    for (index, token) in tokens.iter().enumerate() {
        let word = clean(token).to_lowercase();
        // "last 10" is a count *and* an order: either way the word says "the newest ones".
        let asks_newest = matches!(word.as_str(), "newest" | "latest" | "recent" | "last");
        if asks_newest {
            sort = Some("newest");
            used[index] = true;
        } else if matches!(word.as_str(), "oldest" | "earliest") {
            sort = Some("oldest");
            used[index] = true;
        } else if matches!(word.as_str(), "alphabetical" | "alphabetically") {
            sort = Some("title");
            used[index] = true;
        }
    }

    // 2. Who it is about: "Mehmet's", "assigned to Mehmet", "by Mehmet".
    let mut assignee: Option<String> = None;
    for index in 0..tokens.len() {
        if used[index] {
            continue;
        }
        if let Some(name) = possessive(tokens[index]) {
            assignee = Some(name.to_owned());
            used[index] = true;
            continue;
        }
        let word = clean(tokens[index]).to_lowercase();
        let points_at_a_name = matches!(word.as_str(), "by" | "assigned" | "assignee" | "owner");
        if points_at_a_name {
            let next = tokens
                .get(index + 1)
                .map(|token| clean(token))
                .unwrap_or("");
            let next_is_name = next.chars().count() >= 2
                && !STOPWORDS.contains(&next.to_lowercase().as_str())
                && !DOMAINS
                    .iter()
                    .any(|(needle, _)| *needle == next.to_lowercase());
            if next_is_name {
                if word == "assigned"
                    && clean(tokens.get(index + 1).copied().unwrap_or("")).to_lowercase() == "to"
                {
                    // "assigned to X": the name is one word further along.
                    if let Some(name) = tokens.get(index + 2).map(|token| clean(token)) {
                        if name.chars().count() >= 2 {
                            assignee = Some(name.to_owned());
                            used[index] = true;
                            used[index + 1] = true;
                            used[index + 2] = true;
                        }
                    }
                    continue;
                }
                assignee = Some(next.to_owned());
                used[index] = true;
                used[index + 1] = true;
            }
        }
    }
    if let Some(name) = &assignee {
        filters.push(IntentFilter {
            key: "assignee",
            label: "assignee",
            value: name.clone(),
        });
    }

    // 3. How many: "last 10", "top 5", "10 pages".
    let mut limit: Option<u32> = None;
    for index in 0..tokens.len() {
        if used[index] {
            continue;
        }
        if let Some(value) = small_number(clean(tokens[index])) {
            let followed_by_a_count_word = index > 0
                && matches!(
                    clean(tokens[index - 1]).to_lowercase().as_str(),
                    "last" | "latest" | "recent" | "top" | "first"
                );
            let followed_by_a_domain = tokens
                .get(index + 1)
                .and_then(|token| domain_of(clean(token)))
                .is_some();
            if followed_by_a_count_word || followed_by_a_domain || tokens.len() > 1 {
                limit = Some(value);
                used[index] = true;
            }
        }
    }
    // The count stays in its own field: the preview prints it once ("last 10") and the results
    // screen has no filter of that name to apply.

    // 4. The verb: what the reader wants done with what they named.
    let mut asked_for_an_action = false;
    for (index, token) in tokens.iter().enumerate() {
        if used[index] {
            continue;
        }
        if VERBS.contains(&clean(token).to_lowercase().as_str()) {
            asked_for_an_action = true;
            used[index] = true;
        }
    }

    // 5. The content words: everything the phrase was actually about.
    let mut content: Vec<String> = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        if used[index] {
            continue;
        }
        let word = clean(token);
        let lowered = word.to_lowercase();
        if word.chars().count() < 2 || STOPWORDS.contains(&lowered.as_str()) {
            continue;
        }
        content.push(word.to_owned());
    }

    // 6. The domain, when the words name one the index answers for.
    let mut entity: Option<String> = None;
    let mut provider: Option<&'static str> = None;
    for word in &content {
        if let Some(key) = domain_of(word) {
            if provider.is_none_or(|current| provider_rank(key) < provider_rank(current)) {
                provider = Some(key);
            }
            if entity.is_none() {
                entity = Some(word.clone());
            }
        }
    }
    if entity.is_none() {
        // A word that is not a known domain is still what the phrase was about ("tickets");
        // the interpretation stays honest about having no provider for it.
        entity = content.first().cloned();
    }
    if let Some(name) = &assignee {
        // The name is about the search too — it is what the reader is looking for.
        if !content.iter().any(|word| word.eq_ignore_ascii_case(name)) {
            content.push(name.clone());
        }
    }

    let query = content.join(" ");

    // 7. A command, when the words name one and the phrase asked for no filtering: a phrase that
    //    carries an order or a count is a search, not a trip to a screen.
    let filtered = !filters.is_empty() || sort.is_some();
    let command = if filtered {
        None
    } else {
        command_for(&content)
    };

    let mut intent = Intent {
        kind: IntentKind::Unclear,
        entity,
        provider,
        command_id: command.map(|spec| spec.id),
        query,
        filters,
        sort,
        limit,
        confidence: 0.0,
    };

    // 8. Confidence: each recognised part raises it; an unmapped domain cannot reach the top.
    let mut confidence = 0.30_f32;
    if asked_for_an_action {
        confidence += 0.20;
    }
    if intent.entity.is_some() {
        confidence += 0.25;
    }
    if intent.provider.is_some() {
        confidence += 0.10;
    }
    if !intent.filters.is_empty() || intent.sort.is_some() {
        confidence += 0.10;
    }
    if intent.command_id.is_some() {
        confidence += 0.15;
        intent.kind = IntentKind::Command;
    } else if intent.provider.is_some()
        || !intent.filters.is_empty()
        || intent.sort.is_some()
        || intent.limit.is_some()
        || content_len(&intent.query) > 1
    {
        // A reading is a search when it is more than one unknown word on its own: a lone noun the
        // platform knows nothing about is "unclear" and gets alternatives instead of a promise.
        intent.kind = IntentKind::Search;
    }
    intent.confidence = confidence.min(0.95).round_to_hundredths();

    intent
}

/// The command whose own words cover the phrase's content words, if any.
///
/// The registry's order breaks ties, so the answer is stable; a single word has to match a
/// keyword or alias rather than prose, so "sample" never becomes "Open pages".
fn command_for(content: &[String]) -> Option<&'static CommandSpec> {
    if content.is_empty() {
        return None;
    }
    let lowered: Vec<String> = content.iter().map(|word| word.to_lowercase()).collect();

    commands::COMMANDS.iter().find(|spec| {
        let haystack = [spec.title, spec.id, spec.hint]
            .iter()
            .map(|value| value.to_lowercase())
            .collect::<Vec<_>>()
            .join(" ");
        let words = spec
            .keywords
            .iter()
            .chain(spec.aliases.iter())
            .map(|value| value.to_lowercase())
            .collect::<Vec<_>>()
            .join(" ");
        lowered
            .iter()
            .all(|word| haystack.contains(word.as_str()) || words.contains(word.as_str()))
    })
}

/// What else the words could have meant, best first.
///
/// Two sources, and only two: the commands this caller may run (their own screens) and the
/// providers the index answers for. An alternative is always a **screen** — never an action — so
/// a suggestion can never run something by being clicked.
#[must_use]
pub fn alternatives(intent: &Intent, allows: &dyn Fn(&str) -> bool) -> Vec<Alternative> {
    let words: Vec<String> = intent
        .query
        .split_whitespace()
        .map(|word| word.to_lowercase())
        .collect();
    let mut found: Vec<(u8, usize, Alternative)> = Vec::new();

    // The screens whose own words the phrase touched.
    for (index, spec) in commands::COMMANDS.iter().enumerate() {
        if spec.kind() != CommandKind::Navigate || !spec.permission.is_none_or(allows) {
            continue;
        }
        if matches!(intent.command_id, Some(id) if id == spec.id) {
            continue;
        }
        let haystack = [spec.title, spec.hint]
            .iter()
            .map(|value| value.to_lowercase())
            .chain(spec.keywords.iter().map(|value| value.to_lowercase()))
            .chain(spec.aliases.iter().map(|value| value.to_lowercase()))
            .collect::<Vec<_>>()
            .join(" ");
        let hits = words
            .iter()
            .filter(|word| word.chars().count() >= 3 && haystack.contains(word.as_str()))
            .count();
        if hits > 0 {
            let score = (120 + (hits.min(3) as u8) * 20).min(100);
            found.push((
                score,
                index,
                Alternative {
                    label: spec.title.to_owned(),
                    kind: "command",
                    route: spec.route.to_owned(),
                    provider: None,
                    command_id: Some(spec.id),
                    confidence: (0.4 + 0.15 * hits as f32).min(0.8),
                },
            ));
        }
    }

    // The one provider the phrase's own words name but the reading did not use.
    if intent.provider.is_none() {
        for (index, spec) in PROVIDERS.iter().enumerate() {
            let haystack = format!("{} {} {}", spec.key, spec.title, spec.hint).to_lowercase();
            if words
                .iter()
                .any(|word| word.chars().count() >= 3 && haystack.contains(word.as_str()))
            {
                found.push((
                    90,
                    100 + index,
                    Alternative {
                        label: format!(
                            "Search {} for “{}”",
                            spec.title,
                            if intent.query.trim().is_empty() {
                                intent.entity.clone().unwrap_or_default()
                            } else {
                                intent.query.clone()
                            }
                        ),
                        kind: "search",
                        route: search_url_with(provider_search_words(intent), Some(spec.key), None),
                        provider: Some(spec.key),
                        command_id: None,
                        confidence: 0.5,
                    },
                ));
            }
        }
    }

    // Every reading keeps one way out that cannot be wrong: the words themselves.
    if !intent.query.trim().is_empty() || intent.entity.is_some() {
        found.push((
            10,
            usize::MAX,
            Alternative {
                label: format!("Search everything for “{}”", search_words(intent)),
                kind: "search",
                route: search_url_with(search_words(intent), None, None),
                provider: None,
                command_id: None,
                confidence: 0.4,
            },
        ));
    }

    found.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    let mut rows: Vec<Alternative> = Vec::new();
    for (_, _, alternative) in found {
        if rows
            .iter()
            .any(|row| row.route == alternative.route || row.label == alternative.label)
        {
            continue;
        }
        rows.push(alternative);
        if rows.len() >= MAX_ALTERNATIVES {
            break;
        }
    }
    rows
}

/// The words an alternative's own search carries.
fn search_words(intent: &Intent) -> String {
    if intent.query.trim().is_empty() {
        intent.entity.clone().unwrap_or_default()
    } else {
        intent.query.clone()
    }
}

/// The words a provider-narrowed alternative search carries.
fn provider_search_words(intent: &Intent) -> String {
    search_words(intent)
}

/// One search URL built from its own pieces (`/search?q=…&type=…`).
fn search_url_with(words: String, provider: Option<&str>, sort: Option<&str>) -> String {
    let mut url = String::from("/search?q=");
    url.push_str(&encode_component(&words));
    if let Some(provider) = provider {
        url.push_str("&type=");
        url.push_str(provider);
    }
    if let Some(sort) = sort {
        url.push_str("&sort=");
        url.push_str(sort);
    }
    url
}

/// Percent-encode one query-string component (`/` and `&` inside words must not split the URL).
///
/// A small encoder on purpose: the crate carries no URL dependency, and the alphabet a search box
/// produces is exactly the one this covers.
#[must_use]
pub fn encode_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// `f32` rounding to the hundredth, so confidence prints the same everywhere.
trait RoundToHundredths {
    fn round_to_hundredths(self) -> f32;
}

impl RoundToHundredths for f32 {
    fn round_to_hundredths(self) -> f32 {
        (self * 100.0).round() / 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A caller holding exactly `keys`.
    fn allows(keys: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |key: &str| keys.contains(&key)
    }

    #[test]
    fn an_empty_phrase_reads_as_nothing() {
        let intent = parse("   ");
        assert_eq!(intent.kind, IntentKind::Unclear);
        assert_eq!(intent.confidence, 0.0);
        assert!(!intent.is_runnable(&allows(&[])));
        assert!(intent.preview().contains("Search for"));
    }

    #[test]
    fn the_requests_own_phrase_reads_as_the_request_says_it_should() {
        let intent = parse("Open Mehmet's last 10 tickets");
        assert_eq!(intent.entity.as_deref(), Some("tickets"));
        assert_eq!(intent.provider, None, "no tickets provider exists yet");
        assert_eq!(intent.limit, Some(10));
        assert_eq!(intent.sort, Some("newest"));
        assert_eq!(
            intent
                .filters
                .iter()
                .find(|filter| filter.key == "assignee")
                .map(|filter| filter.value.as_str()),
            Some("Mehmet")
        );
        assert_eq!(
            intent.preview(),
            "Tickets · assignee: Mehmet · last 10 · newest first"
        );
        // The domain the panel has no screen for is not runnable: the words stay a search.
        assert!(!intent.is_runnable(&allows(&["search.read"])));
        assert_eq!(intent.kind, IntentKind::Search);
        assert!(intent.confidence >= 0.75, "{}", intent.confidence);
    }

    #[test]
    fn a_phrase_naming_a_domain_the_index_has_is_runnable() {
        let intent = parse("show me the newest pages");
        assert_eq!(intent.kind, IntentKind::Search);
        assert_eq!(intent.provider, Some("pages"));
        assert_eq!(intent.sort, Some("newest"));
        assert!(intent.is_runnable(&allows(&["search.read", "content.pages.read"])));
        assert_eq!(
            intent.search_url(),
            "/search?q=pages&type=pages&sort=newest"
        );
        assert!(intent.preview().contains("Pages"));
        assert!(intent.preview().contains("newest first"));
    }

    #[test]
    fn a_phrase_naming_a_command_reads_as_that_command_and_keeps_its_own_words() {
        let intent = parse("open media");
        assert_eq!(intent.kind, IntentKind::Command);
        assert_eq!(intent.command_id, Some("nav.media"));
        assert!(intent.is_runnable(&allows(&["search.read", "media.read"])));
        assert!(!intent.runs_through_a_service());
        assert_eq!(intent.command_route(), Some("/media"));
        assert_eq!(intent.preview(), "Open media");

        let action = parse("rebuild the search index");
        assert_eq!(action.kind, IntentKind::Command);
        assert_eq!(action.command_id, Some("act.reindex-search"));
        assert!(action.runs_through_a_service());
        assert!(action.preview().contains("asks first"));
    }

    #[test]
    fn a_phrase_asking_for_an_order_is_a_search_not_a_trip_to_a_screen() {
        // "newest pages" names a command's own words, but a reader who asked for an order is
        // filtering a result set, not opening a list.
        let intent = parse("newest pages");
        assert_eq!(intent.kind, IntentKind::Search);
        assert_eq!(intent.provider, Some("pages"));
        assert_eq!(intent.command_id, None);
    }

    #[test]
    fn gibberish_reads_as_unclear_and_never_runnable() {
        let intent = parse("zzqqxx");
        assert_eq!(intent.kind, IntentKind::Unclear);
        assert!(!intent.is_runnable(&allows(&["search.read"])));
        assert!(intent.confidence < RUN_CONFIDENCE, "{}", intent.confidence);
        assert_eq!(intent.entity.as_deref(), Some("zzqqxx"));
    }

    #[test]
    fn a_three_digit_number_is_a_search_word_not_a_count() {
        let intent = parse("open invoice 4812");
        assert_eq!(intent.limit, None, "a four-digit number is an identifier");
        assert!(intent.query.contains("4812"), "{}", intent.query);
    }

    #[test]
    fn the_domain_maps_through_the_provider_table_and_nothing_else() {
        assert_eq!(domain_of("Files"), Some("media"));
        assert_eq!(domain_of("kittens"), None);
        assert_eq!(parse("list uploads").provider, Some("media"));
        assert_eq!(parse("list kittens").provider, None);
    }

    #[test]
    fn a_command_the_caller_may_not_run_degrades_to_the_search() {
        let mut intent = parse("open sites");
        assert_eq!(intent.command_id, Some("nav.sites"));
        intent.keep_only_commands(&allows(&["search.read"]));
        assert_eq!(intent.command_id, None);
        assert_eq!(intent.kind, IntentKind::Search);
        assert!(!intent.is_runnable(&allows(&["search.read"])));
        assert_eq!(
            intent.provider,
            Some("sites"),
            "the words still name a domain"
        );

        let mut allowed = parse("open sites");
        allowed.keep_only_commands(&allows(&["sites.read"]));
        assert_eq!(allowed.command_id, Some("nav.sites"));
    }

    #[test]
    fn alternatives_are_screens_the_caller_may_open_and_always_offer_the_words() {
        let intent = parse("zzqqxx");
        let rows = alternatives(&intent, &allows(&["search.read"]));
        assert!(!rows.is_empty(), "an unclear phrase still gets a way out");
        assert!(rows.iter().all(|row| row.route.starts_with('/')));
        assert!(rows[0].label.contains("zzqqxx"));

        // A member without `content.pages.read` is never offered the pages screen.
        let member = alternatives(&parse("ticket 4812"), &allows(&["search.read"]));
        assert!(
            member.iter().all(|row| row.command_id != Some("nav.pages")),
            "a screen the caller cannot open is not suggested"
        );
    }

    #[test]
    fn confidence_is_computed_from_what_was_recognised() {
        let bare = parse("pages").confidence;
        let with_verb = parse("open pages").confidence;
        let with_more = parse("open newest pages").confidence;
        assert!(bare < with_verb, "{bare} !< {with_verb}");
        assert!(with_verb <= with_more, "{with_verb} !<= {with_more}");
        assert!(parse("open the newest pages in the acme site").confidence <= 0.95);
    }

    #[test]
    fn encoding_keeps_the_url_in_one_piece() {
        assert_eq!(encode_component("mehmet tickets"), "mehmet+tickets");
        assert_eq!(encode_component("a&b=c"), "a%26b%3Dc");
        assert_eq!(encode_component("ödeme/plan"), "%C3%B6deme%2Fplan");
    }

    #[test]
    fn a_search_url_never_carries_a_provider_the_index_does_not_answer_for() {
        let intent = parse("open Mehmet's last 10 tickets");
        let url = intent.search_url();
        assert!(url.starts_with("/search?q="), "{url}");
        assert!(url.contains("sort=newest"), "{url}");
        assert!(!url.contains("type="), "{url}");
    }
}
