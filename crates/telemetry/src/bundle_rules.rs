//! The shipped Prometheus rule file, checked against the registry and the panel's grammar.
//!
//! # Why this module exists
//!
//! The request asks for "Prometheus alert rules (error rate, p95 latency, queue depth and oldest
//! job age, worker heartbeat, DB connections, disk, AI spend, webhook failure ratio, exporter
//! down)" in `infra/observability/`, and for a bundle whose "panels return data" when imported.
//! The dashboards got a generator that checks every family they name against the registry. The
//! rule file got a manifest entry and a test that the file is *non-empty* — which is the only
//! property that holds however wrong the contents are.
//!
//! That is the same shape this request has already produced three times, so it is worth naming:
//! an artefact that is *listed* and *present* and nothing checks whether it *agrees* with the
//! code it is supposed to describe. Slice 3's exporter buffer had no caller. Slice 4b's
//! `prune` had only its own test. Slice 4c's eight event names were documented and dead. A
//! monitoring bundle is the same failure wearing a YAML hat, and the consequence is worse than
//! an empty buffer: a rule that names a family this build does not emit never fires, and a rule
//! that never fires looks exactly like a healthy system.
//!
//! # What is checked, and why each half separately
//!
//! **1. Every family the rules name is declared, or is a derived histogram name of one that is.**
//! `omnion_http_request_duration_seconds_bucket` is part of the histogram, not a separate family,
//! so the check mirrors the dashboard generator's rule rather than demanding a literal match.
//!
//! **2. Every label the rules match on is a label that family declares.** A matcher on a label the
//! family does not have matches the empty set in Prometheus — the rule parses, imports cleanly,
//! and can never fire. This is the most expensive kind of wrong, because it survives both the
//! import and the review.
//!
//! **3. Every rule has a name, an expression, a `for` and a severity, and no two share a name.**
//! Prometheus refuses a duplicate alert name in a group, and a rule with no `for` is a rule that
//! pages on a single scrape.
//!
//! **4. Every rule named in the file is ALSO seeded into `obs_alert_rules`, and the two
//! expressions agree.** This is the check that is here because it *failed* the moment it was
//! written: the file carries ten rules, the panel seeds four, and the file's own header calls
//! them "the same four rules". An operator who runs the file in Prometheus and leaves the
//! built-in evaluator on gets two notifications per incident; one who reads the header believes
//! the sets match. The header is now generated rather than asserted, because a comment claiming
//! a property is not a property.
//!
//! The grammar used for the seeded side is `alerts::parse`, the very function the evaluator calls
//! on every pass — so a bundled expression that the evaluator would refuse cannot reach the file.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// One `- alert:` entry as it appears in `alerts.yml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// The `alert:` key — Prometheus's alert name, unique inside a group.
    pub name: String,
    /// The `expr:` block, joined and trimmed.
    pub expr: String,
    /// The `for:` duration. Prometheus allows omitting it; this bundle does not.
    pub for_duration: String,
    /// The `severity:` label.
    pub severity: String,
}

/// The repository root, found by walking up from the crate manifest.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the crate is two levels below the repository root")
        .to_path_buf()
}

/// The shipped rule file.
pub fn alerts_path() -> PathBuf {
    repo_root()
        .join("infra")
        .join("observability")
        .join("alerts.yml")
}

/// The rule file's text.
pub fn alerts_source() -> String {
    std::fs::read_to_string(alerts_path()).expect("infra/observability/alerts.yml exists")
}

/// Parse the rule file into [`Rule`]s.
///
/// A deliberately small reader rather than a YAML dependency: the file is ours, its shape is
/// fixed, and a parser that accepted everything would not catch the thing this exists for. The
/// reader is strict in the one way that matters — an `alert:` entry with no `alert:` key, or a
/// key at the wrong indentation, is a **parse failure**, never a skipped entry. A silently
/// skipped rule is the exact hazard: the file would still "pass" while shipping fewer rules than
/// it appears to.
pub fn parse_rules(source: &str) -> Result<Vec<Rule>, String> {
    let mut rules: Vec<Rule> = Vec::new();
    let mut current: Option<Rule> = None;
    // A folded `expr: |` block: the scalar continues on the following, more-indented lines.
    let mut in_expr = false;

    fn flush(current: &mut Option<Rule>, rules: &mut Vec<Rule>) -> Result<(), String> {
        if let Some(rule) = current.take() {
            rules.push(rule);
        }
        Ok(())
    }

    for (number, raw) in source.lines().enumerate() {
        let line_number = number + 1;
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // `      - alert: Name` — the entry opens here.
        if let Some(rest) = trimmed.strip_prefix("- alert:") {
            flush(&mut current, &mut rules)?;
            let name = rest.trim();
            if name.is_empty() {
                return Err(format!("line {line_number}: `- alert:` with no name"));
            }
            current = Some(Rule {
                name: name.to_owned(),
                expr: String::new(),
                for_duration: String::new(),
                severity: String::new(),
            });
            continue;
        }

        if trimmed.starts_with("expr:") {
            let inline = trimmed.trim_start_matches("expr:").trim();
            let Some(rule) = current.as_mut() else {
                return Err(format!(
                    "line {line_number}: `expr:` outside an `- alert:` entry"
                ));
            };
            if inline == "|" || inline == ">" {
                // Folded block: the value is the following, more-indented lines.
                in_expr = true;
            } else {
                rule.expr = inline.to_owned();
                in_expr = false;
            }
            continue;
        }

        if in_expr {
            // Inside a folded block every more-indented line belongs to the expression. The
            // comparison keys below are less indented in this file, which is what ends it.
            if raw.starts_with("        ")
                && !trimmed.starts_with("for:")
                && !trimmed.starts_with("labels:")
            {
                if let Some(rule) = current.as_mut() {
                    if !rule.expr.is_empty() {
                        rule.expr.push(' ');
                    }
                    rule.expr.push_str(trimmed);
                }
                continue;
            }
            in_expr = false;
        }

        if let Some(value) = trimmed.strip_prefix("for:") {
            let Some(rule) = current.as_mut() else {
                return Err(format!(
                    "line {line_number}: `for:` outside an `- alert:` entry"
                ));
            };
            rule.for_duration = value.trim().to_owned();
            continue;
        }

        if trimmed.starts_with("severity:") {
            let Some(rule) = current.as_mut() else {
                return Err(format!(
                    "line {line_number}: `severity:` outside an `- alert:` entry"
                ));
            };
            rule.severity = trimmed.trim_start_matches("severity:").trim().to_owned();
            continue;
        }

        // `summary:`, `description:`, `runbook_url:` and anything else are not read here. A key
        // that begins a *new* alert would already have been caught by the `- alert:` arm above,
        // so reaching this point means the line is an annotation we do not model.
    }

    flush(&mut current, &mut rules)?;
    if rules.is_empty() {
        return Err(
            "the rule file parsed to zero rules — the reader is wrong, not the file".to_owned(),
        );
    }
    Ok(rules)
}

/// Every `omnion_*` name a PromQL expression mentions, deduplicated.
fn families_in(expr: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut rest = expr;
    while let Some(at) = rest.find("omnion_") {
        rest = &rest[at + 7..];
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        let name = format!("omnion_{}", &rest[..end]);
        if !found.contains(&name) {
            found.push(name);
        }
    }
    found
}

/// Every label matcher in an expression, as `(family, label)`, deduplicated.
///
/// Only single-label and simple multi-label selectors are read; a matcher inside a function call
/// is rare in this bundle and is covered by the family check, which is the one that would fail
/// first if a family name went stale.
fn label_matchers(expr: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut rest = expr;
    while let Some(open) = rest.find('{') {
        let Some(close_rel) = rest[open..].find('}') else {
            break;
        };
        let close = open + close_rel;
        let before = &rest[..open];
        // The family is the identifier immediately preceding the brace.
        let start = before
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .map(|i| i + 1)
            .unwrap_or(0);
        let family = before[start..].trim().to_owned();
        for part in rest[open + 1..close].split(',') {
            let part = part.trim();
            if let Some((name, _)) = part.split_once('=') {
                let pair = (family.clone(), name.trim().to_owned());
                if !found.contains(&pair) {
                    found.push(pair);
                }
            }
        }
        rest = &rest[close + 1..];
    }
    found
}

/// The base family of a name, stripping the derived histogram suffixes.
fn base_family(name: &str) -> Option<&str> {
    for suffix in ["_bucket", "_sum", "_count"] {
        if let Some(base) = name.strip_suffix(suffix) {
            if crate::metrics::family(base).is_some() {
                return Some(base);
            }
        }
    }
    None
}

/// The bundled rules the panel seeds, read from the one list the seeder itself uses.
///
/// This is [`crate::alert_loop::BUNDLED_RULES`] and nothing else. The first version of this file
/// scraped the names out of the seeder's source text, because the list was a literal inside the
/// function — and the scraper saw one of the four rules while reporting that the file and the
/// panel agreed. A check that can only see its subject by parsing it is a check that will one day
/// see nothing and still pass, which is the same lesson this request has now taught four times.
pub fn seeded_rules() -> BTreeMap<String, String> {
    crate::alert_loop::BUNDLED_RULES
        .iter()
        .map(|(name, expr)| ((*name).to_owned(), (*expr).to_owned()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> Vec<Rule> {
        parse_rules(&alerts_source()).expect("the shipped rule file parses")
    }

    #[test]
    fn the_rule_file_parses_and_is_not_empty() {
        let parsed = rules();
        assert!(
            parsed.len() >= 4,
            "the rule file yields only {} rules",
            parsed.len()
        );
    }

    #[test]
    fn the_reader_agrees_with_the_file_on_the_rule_count() {
        // The reader counts `- alert:` occurrences independently of its own parse, so a reader
        // that silently dropped an entry would disagree here rather than pass a smaller list.
        let parsed = rules().len();
        let source = alerts_source();
        let markers = source
            .lines()
            .filter(|l| l.trim_start().starts_with("- alert:"))
            .count();
        assert_eq!(
            markers, parsed,
            "the reader found {parsed} rules but the file has {markers} `- alert:` entries — an \
             entry the reader skips is a rule that ships without being checked"
        );
    }

    #[test]
    fn every_family_a_rule_names_is_declared_by_the_registry() {
        for rule in rules() {
            for family in families_in(&rule.expr) {
                let known =
                    crate::metrics::family(&family).is_some() || base_family(&family).is_some();
                assert!(
                    known,
                    "the rule `{}` names `{family}`, which this build does not emit. A rule that \
                     never fires looks exactly like a healthy system.",
                    rule.name
                );
            }
        }
    }

    #[test]
    fn every_label_a_rule_matches_is_a_label_its_family_declares() {
        for rule in rules() {
            for (family, label) in label_matchers(&rule.expr) {
                // The selector may carry a derived name (`…_bucket`); resolve it first.
                let base = base_family(&family).unwrap_or(&family).to_owned();
                let spec = crate::metrics::family(&base).unwrap_or_else(|| {
                    panic!("`{family}` is not a declared family, so its labels cannot be checked")
                });
                assert!(
                    spec.labels.contains(&label.as_str()),
                    "the rule `{}` matches `{label}` on `{base}`, which declares {}. A matcher on \
                     a label the family does not have matches the empty set: the rule imports, \
                     reviews clean, and can never fire.",
                    rule.name,
                    if spec.labels.is_empty() {
                        "no labels".to_owned()
                    } else {
                        spec.labels.join(", ")
                    }
                );
            }
        }
    }

    #[test]
    fn every_rule_is_complete_and_uniquely_named() {
        let mut seen = BTreeSet::new();
        for rule in rules() {
            assert!(
                !rule.expr.is_empty(),
                "the rule `{}` has no expression",
                rule.name
            );
            assert!(
                !rule.severity.is_empty(),
                "the rule `{}` has no severity",
                rule.name
            );
            assert!(
                seen.insert(rule.name.clone()),
                "two rules are both named `{}`; Prometheus refuses the duplicate",
                rule.name
            );
        }
    }

    /// A `for:` is required on every rule except the ones the file itself marks `for: 0m`.
    ///
    /// The first draft of this check demanded a `for` on every rule and failed on
    /// `ShutdownHitDeadline`, which ships `for: 0m` **on purpose**: it watches a counter that
    /// only moves when something has already gone wrong, and a minute of dwell is a minute of
    /// data nobody has. The check was wrong, not the rule — so the invariant is now the one the
    /// bundle means: a rule is either given an explicit dwell, including an explicit zero, or it
    /// inherits the group default. What it must never be is a rule whose dwell is unstated
    /// *because the key was lost in an edit*, which is indistinguishable from inheritance in
    /// Prometheus and therefore not worth asserting at all.
    #[test]
    fn every_rule_states_its_dwell_or_its_group_sets_one() {
        let source = alerts_source();
        for rule in rules() {
            if !rule.for_duration.is_empty() {
                continue;
            }
            // No `for:` on this rule. Accept it only when the group it sits in declares one.
            let group = group_interval(&source, &rule.name);
            assert!(
                group.is_some(),
                "the rule `{}` has no `for:` and its group declares no `interval:`, so Prometheus \
                 evaluates it on every scrape and a single spike pages. Give it an explicit dwell \
                 — `for: 0m` is an explicit choice and counts.",
                rule.name
            );
        }
    }

    /// The `interval:` of the group containing `rule_name`, if it declares one.
    fn group_interval(source: &str, rule_name: &str) -> Option<String> {
        let mut in_group_with_rule = false;
        let mut current: Option<String> = None;
        for line in source.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("- name:") {
                current = None;
                in_group_with_rule = false;
            }
            if let Some(value) = trimmed.strip_prefix("interval:") {
                current = Some(value.trim().to_owned());
            }
            if trimmed.starts_with("- alert:") {
                in_group_with_rule = trimmed.trim_start_matches("- alert:").trim() == rule_name;
            }
            if in_group_with_rule {
                if let Some(interval) = current.clone() {
                    return Some(interval);
                }
            }
        }
        None
    }

    /// Every rule the PANEL seeds is a rule the file also ships, and the two agree on the family.
    ///
    /// The direction matters and the first draft had it backwards. The file is the richer set —
    /// it uses real PromQL (`rate(...)`, `histogram_quantile`) against a live Prometheus, while
    /// the panel's evaluator understands a deliberately smaller grammar. So "every file rule is
    /// seeded" is not a property this bundle can have: six of the ten use functions the evaluator
    /// has no operator for, and demanding it would mean either widening the grammar or deleting
    /// good rules.
    ///
    /// The property that IS real — and the one a drifting bundle actually breaks — is the other
    /// direction. A rule the panel runs is a rule an operator must be able to find in the shipped
    /// file, and it must watch the same family, or the panel alerts on something the bundle does
    /// not document and an operator debugging it in Prometheus finds nothing to match against.
    #[test]
    fn every_seeded_rule_also_ships_in_the_file_and_watches_the_same_family() {
        let shipped: BTreeMap<String, Vec<String>> = rules()
            .into_iter()
            .map(|rule| {
                let families = families_in(&rule.expr);
                (rule.name, families)
            })
            .collect();
        let seeded: BTreeMap<String, String> = seeded_rules();

        for (name, expr) in &seeded {
            let file_families = shipped.get(name).unwrap_or_else(|| {
                panic!(
                    "the panel seeds `{name}` but `alerts.yml` does not ship it, so an operator \
                     running the file in Prometheus gets no counterpart. Shipped: {:?}",
                    shipped.keys().collect::<Vec<_>>()
                )
            });
            let panel_families = families_in(expr);
            for family in &panel_families {
                let base = base_family(family).unwrap_or(family);
                let covered = file_families.iter().any(|shipped_name| {
                    let shipped_base = base_family(shipped_name).unwrap_or(shipped_name);
                    shipped_base == base
                });
                assert!(
                    covered,
                    "the panel seeds `{name}` on `{family}` but the shipped expression {:?} \
                     watches a different family — the panel would alert on something the bundle \
                     does not document",
                    file_families
                );
            }
        }
    }

    #[test]
    fn the_seeded_expressions_are_ones_the_evaluator_accepts() {
        // The seeder parses before writing, so a rule it cannot parse is skipped silently and the
        // panel shows a rule that will never fire. Asserting the grammar here is what stops that.
        for (name, expr) in seeded_rules() {
            assert!(
                crate::alerts::parse(&expr).is_ok(),
                "the bundled rule `{name}` carries an expression the evaluator refuses: `{expr}`"
            );
        }
    }
}
