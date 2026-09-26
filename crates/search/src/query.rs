//! The query a search runs, and the ranking rule that orders its hits.
//!
//! Everything in this module is pure: parsing a raw query string and turning candidate rows into
//! scored hits are deterministic functions with unit tests, so the ranking rule can be reasoned
//! about (and changed) without a database.
//!
//! The rule, in one paragraph: a query is a list of lower-case terms, and a candidate must
//! contain **all** of them somewhere (title, subtitle or the details a source adds — a slug, a
//! content type, a site name). A term that begins the title is worth the most, then a term that
//! begins a word of the title, then a term anywhere in the title, then the subtitle, then the
//! details; a freshness bonus nudges recently changed rows up. Nothing matches, nothing is
//! returned — a search never answers with "close enough" rows, which is what keeps the palette
//! honest.

use time::OffsetDateTime;

/// Longest accepted query, in characters.
pub const MAX_QUERY_LENGTH: usize = 200;
/// Most terms one query is split into; the rest is ignored.
pub const MAX_TERMS: usize = 8;
/// Longest single term, in characters. A pasted paragraph cannot become a query.
pub const MAX_TERM_LENGTH: usize = 64;
/// Hits per source group when the caller does not ask for a size.
pub const DEFAULT_GROUP_LIMIT: usize = 5;
/// Largest group a caller may ask for.
pub const MAX_GROUP_LIMIT: usize = 20;
/// Candidate rows a single source hands to the ranker before truncation.
pub const MAX_CANDIDATES: i64 = 200;

/// One parsed search query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    raw: String,
    terms: Vec<String>,
}

impl Query {
    /// Parse a raw query string; `None` when there is nothing to search for.
    ///
    /// The raw text is kept (trimmed, capped) for the response so a caller can see what was
    /// understood; the terms are lower-cased, de-duplicated and capped.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        let raw: String = trimmed.chars().take(MAX_QUERY_LENGTH).collect();

        let mut terms: Vec<String> = Vec::new();
        for word in raw.split_whitespace() {
            let term: String = word.to_lowercase().chars().take(MAX_TERM_LENGTH).collect();
            if term.is_empty() || terms.contains(&term) {
                continue;
            }
            terms.push(term);
            if terms.len() == MAX_TERMS {
                break;
            }
        }

        if terms.is_empty() {
            return None;
        }
        Some(Self { raw, terms })
    }

    /// The raw (trimmed, capped) query text.
    #[must_use]
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// The parsed terms, in the order they were written.
    #[must_use]
    pub fn terms(&self) -> &[String] {
        &self.terms
    }

    /// SQL `LIKE` patterns, one per term (`%term%`), with wildcards in the term escaped.
    ///
    /// The patterns are bound as a `text[]` and matched with `like any($n)`; every pattern in
    /// this array is escaped for the default backslash escape character.
    #[must_use]
    pub fn patterns(&self) -> Vec<String> {
        self.terms
            .iter()
            .map(|term| format!("%{}%", escape_like(term)))
            .collect()
    }
}

/// Escape the wildcards of a `LIKE` pattern, so a query containing `%` searches for a literal
/// percent sign instead of matching everything.
#[must_use]
pub fn escape_like(term: &str) -> String {
    let mut escaped = String::with_capacity(term.len());
    for ch in term.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// Lower-case a text and collapse its whitespace, the shape both sides of a comparison use.
#[must_use]
pub fn normalize(text: &str) -> String {
    text.split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// One scored hit of one source.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// Key of the source the hit came from (`pages`).
    pub source: &'static str,
    /// Identifier of the record inside its own domain.
    pub id: String,
    /// What the palette shows first.
    pub title: String,
    /// One supporting line (site, slug, content type …), when the source has one.
    pub subtitle: Option<String>,
    /// Where a click goes: a panel route.
    pub url: String,
    /// Ranking score; only comparable inside one query.
    pub score: f64,
    /// When the record last changed, when the source knows.
    pub updated_at: Option<OffsetDateTime>,
}

/// Score one candidate against a query's terms; `None` when it does not match.
///
/// `details` are extra strings the source considers searchable (slug, content type, site name,
/// status …). Matching is all-terms-must-appear; scoring rewards beginnings.
#[must_use]
pub fn score(
    terms: &[String],
    title: &str,
    subtitle: Option<&str>,
    details: &[&str],
) -> Option<f64> {
    let title_normalized = normalize(title);
    let subtitle_normalized = subtitle.map(normalize).unwrap_or_default();
    let details_normalized = normalize(&details.join(" "));

    let matches = terms.iter().all(|term| {
        title_normalized.contains(term.as_str())
            || subtitle_normalized.contains(term.as_str())
            || details_normalized.contains(term.as_str())
    });
    if !matches {
        return None;
    }

    let mut total = 0.0_f64;
    for term in terms {
        if title_normalized == *term {
            total += 150.0;
        }
        if title_normalized.starts_with(term.as_str()) {
            total += 100.0;
        } else if title_normalized
            .split_whitespace()
            .any(|word| word.starts_with(term.as_str()))
        {
            total += 60.0;
        } else if title_normalized.contains(term.as_str()) {
            total += 40.0;
        }
        if subtitle_normalized.contains(term.as_str()) {
            total += 20.0;
        }
        if details_normalized.contains(term.as_str()) {
            total += 10.0;
        }
    }
    Some(total)
}

/// Bonus a hit earns for being fresh, by age in seconds.
#[must_use]
pub fn recency_bonus(age_seconds: i64) -> f64 {
    const DAY: i64 = 24 * 60 * 60;
    if age_seconds <= DAY {
        12.0
    } else if age_seconds <= 7 * DAY {
        6.0
    } else if age_seconds <= 30 * DAY {
        3.0
    } else {
        0.0
    }
}

/// The freshness bonus of a row changed at `updated_at`, read at `now`.
#[must_use]
pub fn freshness(updated_at: Option<OffsetDateTime>, now: OffsetDateTime) -> f64 {
    match updated_at {
        Some(at) => recency_bonus((now - at).whole_seconds()),
        None => 0.0,
    }
}

/// Order hits by score (desc), then freshness, then title, and keep at most `limit`.
#[must_use]
pub fn sort_and_truncate(mut hits: Vec<Hit>, limit: usize) -> Vec<Hit> {
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.updated_at.cmp(&a.updated_at))
            .then_with(|| a.title.cmp(&b.title))
    });
    hits.truncate(limit);
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(input: &[&str]) -> Vec<String> {
        input.iter().map(|t| (*t).to_owned()).collect()
    }

    #[test]
    fn parse_rejects_the_empty_query() {
        assert!(Query::parse("").is_none());
        assert!(Query::parse("   ").is_none());
        assert!(Query::parse("\t\n").is_none());
    }

    #[test]
    fn parse_caps_and_deduplicates_terms() {
        let query = Query::parse("Release release  notes   of the week and the month and the year")
            .expect("query parses");
        assert_eq!(query.terms()[0], "release");
        assert_eq!(query.terms()[1], "notes");
        assert!(query.terms().len() <= MAX_TERMS);
        assert_eq!(query.terms().iter().filter(|t| *t == "release").count(), 1);
    }

    #[test]
    fn parse_lower_cases_and_keeps_the_raw_text() {
        let query = Query::parse("  Release Notes  ").expect("query parses");
        assert_eq!(query.raw(), "Release Notes");
        assert_eq!(query.terms(), &terms(&["release", "notes"]));
    }

    #[test]
    fn parse_truncates_a_pasted_paragraph() {
        let long = "x".repeat(400);
        let query = Query::parse(&long).expect("query parses");
        assert!(query.raw().chars().count() <= MAX_QUERY_LENGTH);
        assert!(query.terms()[0].chars().count() <= MAX_TERM_LENGTH);
    }

    #[test]
    fn patterns_escape_wildcards() {
        let query = Query::parse("100% _real_").expect("query parses");
        assert_eq!(query.patterns(), vec!["%100\\%%", "%\\_real\\_%"]);
    }

    #[test]
    fn normalize_collapses_whitespace_and_case() {
        assert_eq!(normalize("  Release \t Notes "), "release notes");
    }

    #[test]
    fn score_requires_every_term() {
        let hit = score(&terms(&["release", "notes"]), "Release notes", None, &[]);
        assert!(hit.is_some());
        let miss = score(&terms(&["release", "invoice"]), "Release notes", None, &[]);
        assert!(miss.is_none());
    }

    #[test]
    fn score_rewards_the_beginning_of_the_title() {
        let start = score(&terms(&["release"]), "Release notes", None, &[]).expect("match");
        let word = score(&terms(&["notes"]), "Release notes", None, &[]).expect("match");
        let inside = score(&terms(&["lea"]), "Release notes", None, &[]).expect("match");
        let exact = score(&terms(&["release notes"]), "Release notes", None, &[]).expect("match");
        assert!(start > word, "prefix beats a later word: {start} > {word}");
        assert!(
            word > inside,
            "word prefix beats an inside match: {word} > {inside}"
        );
        assert!(exact > start, "an exact title is the best hit");
    }

    #[test]
    fn score_reads_subtitle_and_details() {
        let subtitle = score(&terms(&["draft"]), "Release notes", Some("draft"), &[]);
        let details = score(&terms(&["news"]), "Release notes", None, &["news", "main"]);
        assert!(subtitle.is_some());
        assert!(details.is_some());
        assert!(subtitle.expect("match") > details.expect("match"));
    }

    #[test]
    fn recency_bonus_shrinks_with_age() {
        assert_eq!(recency_bonus(60), 12.0);
        assert_eq!(recency_bonus(3 * 24 * 60 * 60), 6.0);
        assert_eq!(recency_bonus(14 * 24 * 60 * 60), 3.0);
        assert_eq!(recency_bonus(400 * 24 * 60 * 60), 0.0);
        assert_eq!(recency_bonus(-5), 12.0, "a future stamp counts as fresh");
    }

    #[test]
    fn sort_prefers_score_then_freshness_then_title() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let hit = |title: &str, score: f64, age_days: i64| Hit {
            source: "pages",
            id: title.to_owned(),
            title: title.to_owned(),
            subtitle: None,
            url: "/pages".to_owned(),
            score,
            updated_at: Some(now - time::Duration::days(age_days)),
        };

        let ordered = sort_and_truncate(
            vec![
                hit("Beta", 10.0, 1),
                hit("Alpha", 20.0, 30),
                hit("Gamma", 10.0, 1),
                hit("Delta", 20.0, 1),
            ],
            3,
        );

        let titles: Vec<&str> = ordered.iter().map(|h| h.title.as_str()).collect();
        assert_eq!(titles, vec!["Delta", "Alpha", "Beta"]);
    }
}
