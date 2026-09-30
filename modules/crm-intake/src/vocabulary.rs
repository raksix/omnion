//! The intake vocabulary: the closed lists and the guards over them.
//!
//! The same rule as `crates/notifications/src/vocabulary.rs`, and for the same reason: a
//! `text` column that accepts anything is a column nobody can build a filter out of, and a
//! table of "statuses" nobody can enumerate is a table the inbox cannot group by.
//!
//! **The lists are compile-time, and the duplication in SQL is a test.** SQL cannot import a
//! Rust constant, so the status, decision, kind and dedupe-policy lists are written twice —
//! once here, once in `database/migrations/0055_crm_lead_intake.sql` — and
//! `the_migration_agrees_with_the_lists` reads the migration file itself. Both directions
//! fail quietly without it: a status added to Rust and not to SQL passes every unit test in
//! the crate and is then refused by the database, and a status added to SQL and not to Rust is
//! a row the panel has no label for. Either way it reads as "nothing happened".

/// Every status a lead may hold.
///
/// The order is the order the inbox's filter chips render in and the order the lead detail's
/// conversion stepper reads, so "the third status" is one thing rather than two.
///
/// `converted`, `duplicate`, `spam` and `rejected` are terminal: the SLA clock reads only the
/// rows that are still waiting, and a clock that keeps running against a lead somebody marked
/// as spam is a breach notification nobody can act on.
pub const STATUSES: [&str; 8] = [
    "new",
    "assigned",
    "contacted",
    "qualified",
    "converted",
    "duplicate",
    "spam",
    "rejected",
];

/// The verdict a dedupe pass reached.
///
/// `linked` means the lead now belongs to an existing contact, `created` means a new contact
/// was made for it, `duplicate` means the row is kept but *not* linked, and the last two mean
/// the submission was discarded — with its reason, and visible in the inbox.
pub const DECISIONS: [&str; 5] = ["linked", "created", "duplicate", "rejected", "spam"];

/// What kind of surface a source binds.
pub const SOURCE_KINDS: [&str; 3] = ["form", "endpoint", "import"];

/// How a source treats a submission that matches an existing contact.
pub const DEDUPE_POLICIES: [&str; 3] = ["link", "create_anyway", "reject_duplicate"];

/// Where an assignment rule hands a matching lead (slice 2).
///
/// `queue` is deliberately one of the three rather than "no target": a rule that *chose* the
/// unassigned queue is visible on the lead's timeline and in the rule table, while a lead
/// nobody claimed is a different thing entirely. Keeping them as one vocabulary means the
/// rule editor and the evaluator cannot offer a target the other cannot honour.
pub const ASSIGNMENT_TARGETS: [&str; 3] = ["user", "pool", "queue"];

/// The largest page of leads one inbox read may return.
pub const MAX_PAGE: i64 = 100;

/// The largest number of ids one bulk action may name.
pub const MAX_BULK_IDS: usize = 200;

/// The largest submission the platform will accept, in bytes.
///
/// A refused submission is *refused*, not truncated: a truncated payload produces a lead whose
/// fields do not match what the visitor sent, which is a worse failure than a visible error
/// because nobody can see it.
pub const MAX_PAYLOAD_BYTES: usize = 262_144;

/// How many submissions **one address** may send to one source in an hour.
///
/// The second ceiling, and deliberately not a source setting: a per-address limit is a
/// property of abuse, not of the business. An operator with a busy form and a bot hammering
/// the same source has one dial today (`rate_limit_per_hour`) and it moves both at once —
/// lowering it stops the flood and their real enquiries with it. This is that second dial.
///
/// The number is the one a person would pick: nobody types a company name and a message
/// eleven times in sixty minutes, and a form's own ceiling is almost always higher. A
/// visitor who genuinely sends more than this is a caller the source's own `rate_limit_per_hour`
/// will still answer — so the two ceilings compose rather than compete, and this one refuses
/// first because it is the cheaper signal to explain ("this address sent eleven in an hour").
pub const MAX_SUBMISSIONS_PER_ADDRESS_PER_HOUR: i64 = 10;

/// `true` when `value` is a status the platform knows.
#[must_use]
pub fn is_status(value: &str) -> bool {
    STATUSES.contains(&value)
}

/// `true` when `value` is a dedupe decision the platform knows.
#[must_use]
pub fn is_decision(value: &str) -> bool {
    DECISIONS.contains(&value)
}

/// `true` when `value` is a source kind the platform knows.
#[must_use]
pub fn is_source_kind(value: &str) -> bool {
    SOURCE_KINDS.contains(&value)
}

/// `true` when `value` is a dedupe policy the platform knows.
#[must_use]
pub fn is_dedupe_policy(value: &str) -> bool {
    DEDUPE_POLICIES.contains(&value)
}

/// `true` when `value` is a target an assignment rule may hand a lead to.
#[must_use]
pub fn is_round_robin_target(value: &str) -> bool {
    ASSIGNMENT_TARGETS.contains(&value)
}

/// `true` when the lead is still waiting for somebody.
///
/// This is the single definition of "open" in the crate: the SLA sweep, the inbox count and
/// the breached badge all use it, so a lead cannot be counted as waiting in one place and as
/// closed in another.
#[must_use]
pub fn is_open(status: &str) -> bool {
    matches!(status, "new" | "assigned" | "contacted" | "qualified")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lists_have_no_duplicates() {
        for list in [
            &STATUSES[..],
            &DECISIONS[..],
            &SOURCE_KINDS[..],
            &DEDUPE_POLICIES[..],
            &ASSIGNMENT_TARGETS[..],
        ] {
            let mut sorted = list.to_vec();
            sorted.sort_unstable();
            let before = sorted.len();
            sorted.dedup();
            assert_eq!(before, sorted.len(), "duplicate in {list:?}");
        }
    }

    #[test]
    fn the_guards_agree_with_the_lists() {
        for value in STATUSES {
            assert!(is_status(value));
        }
        for value in DECISIONS {
            assert!(is_decision(value));
        }
        for value in SOURCE_KINDS {
            assert!(is_source_kind(value));
        }
        for value in DEDUPE_POLICIES {
            assert!(is_dedupe_policy(value));
        }
        for value in ASSIGNMENT_TARGETS {
            assert!(is_round_robin_target(value));
        }
        assert!(!is_status("won"));
        assert!(!is_decision("maybe"));
        assert!(!is_source_kind("webhook"));
        assert!(!is_dedupe_policy("merge_always"));
        assert!(
            !is_round_robin_target("round_robin"),
            "a target the evaluator cannot honour is a rule that saves and then does nothing"
        );
    }

    #[test]
    fn only_the_four_working_statuses_are_open() {
        // The SLA clock reads exactly this set. If a future status were added to the list and
        // not here, a lead in that status would be counted as waiting by one reader and as
        // closed by another — so the test names each status rather than counting.
        assert!(is_open("new"));
        assert!(is_open("assigned"));
        assert!(is_open("contacted"));
        assert!(is_open("qualified"));
        assert!(!is_open("converted"));
        assert!(!is_open("duplicate"));
        assert!(!is_open("spam"));
        assert!(!is_open("rejected"));
        assert!(!is_open("nonsense"));
    }

    /// The lists, the migrations' check constraints AND the admin panel are the same
    /// closed lists, written three times.
    ///
    /// Each migration is read from the repository rather than pasted here, so the assertion
    /// cannot itself drift out of date — and each is checked against the migration that
    /// *owns* its constraint, because the assignment targets live in the slice-2 file and
    /// asserting them against the slice-1 file would pass for a week after the constraint
    /// was deleted from the database.
    ///
    /// The third source is `apps/admin/lib/crm-intake.ts`, and it is the one most likely to
    /// drift: the panel holds its own `LEAD_STATUSES` because a language boundary cannot
    /// import. Nothing in the TypeScript build can see a Rust constant, so a status added
    /// here and not in the panel compiles cleanly on both sides and shows up in the product
    /// as a lead whose status pill renders raw `snake_case`, or as a filter chip the server
    /// quietly refuses. The panel is read as TEXT rather than executed — the check is
    /// "does this file list exactly these values", which survives reformatting, whereas a
    /// test that ran the module's own extractor would only prove the extractor agrees with
    /// itself.
    #[test]
    fn the_panel_agrees_with_the_crate() {
        let panel = read_panel("lib/crm-intake.ts");
        for (constant, list) in [
            ("LEAD_STATUSES", &STATUSES[..]),
            ("SOURCE_KINDS", &SOURCE_KINDS[..]),
            ("DEDUPE_POLICIES", &DEDUPE_POLICIES[..]),
        ] {
            // Set equality, not `panel.contains(needle)`. A `contains` check passes when the
            // panel lists a strict PREFIX of the crate's list, which is the more likely
            // half-drift: the ninth status someone adds in Rust is the one nobody copies,
            // and "the first eight are there" is exactly what `contains` rewards.
            let found = read_string_set(
                &panel,
                &format!("export const {constant}"),
                "a vocabulary the panel no longer declares is a vocabulary it cannot render",
            );
            let expected: std::collections::BTreeSet<&str> = list.iter().copied().collect();
            assert_eq!(
                found,
                expected.into_iter().map(str::to_string).collect::<std::collections::BTreeSet<_>>(),
                "the panel's {constant} and the crate's list are different vocabularies — a \
                 status the platform accepts and the panel does not know is a lead rendered as \
                 raw snake_case, and a status the panel offers that the database refuses is a \
                 filter that silently returns nothing"
            );
        }

        // The label and tone maps are keyed by the same vocabulary, and a status missing from
        // one of them is the *visible* half of the same drift: the pill falls back to
        // `bg-quiet-soft text-muted` and the chip to the raw value, so the lead still lists and
        // every row of it reads wrong. Both maps are checked for totality, not equality --
        // their VALUES are panel design, and a test asserting them would freeze a colour.
        for constant in ["LEAD_STATUS_LABEL", "LEAD_STATUS_TONE"] {
            let keys = read_object_keys(
                &panel,
                constant,
                "a status the panel has no wording for is a status an operator reads as a \
                 snake_case token",
            );
            for status in STATUSES {
                assert!(
                    keys.contains(status),
                    "{constant} has no entry for {status} — the lead still lists, and every \
                     surface that shows it falls back to raw text"
                );
            }
            assert_eq!(
                keys.len(),
                STATUSES.len(),
                "{constant} carries a key the crate does not have, so the two will drift again"
            );
        }

        // The panel's open/closed partition is the crate's `is_open`, so the two halves are
        // read out of the panel's own source and compared value by value. Deriving the
        // complement here instead would test `is_open` against itself.
        let closed = read_string_set(
            &panel,
            "export const CLOSED_LEAD_STATUSES",
            "the panel classifies a status nowhere; every chip it renders reads one of the two",
        );
        for status in STATUSES {
            let expected = is_open(status);
            assert_eq!(
                closed.contains(status),
                !expected,
                "the panel and the crate disagree about whether {status} is open — one of them \
                 is counting a filed verdict as work that still needs somebody"
            );
        }
        assert_eq!(
            closed.len(),
            STATUSES.iter().filter(|s| !is_open(s)).count(),
            "the panel's closed set names a status the crate does not have, or misses one it does"
        );
    }

    /// The keys of an exported object literal, read out of the panel's source.
    fn read_object_keys(
        panel: &str,
        constant: &str,
        why: &str,
    ) -> std::collections::BTreeSet<String> {
        let start = panel
            .find(&format!("export const {constant}"))
            .unwrap_or_else(|| panic!("{why} — the panel no longer exports {constant}"));
        let rest = &panel[start..];
        let open = rest
            .find('{')
            .unwrap_or_else(|| panic!("{why} — {constant} is not an object literal"));
        let mut depth = 0usize;
        let mut end = None;
        for (index, ch) in rest[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(index);
                        break;
                    }
                }
                _ => {}
            }
        }
        let end = end.unwrap_or_else(|| panic!("{why} — {constant} has an unterminated object"));
        rest[open + 1..open + end]
            .lines()
            .filter_map(|line| object_key(line.trim()))
            .collect()
    }

    /// `new: "New"` -> `new`. A comment line has no `:` and yields nothing, so a
    /// documentation line above an entry is not read as a key of its own.
    fn object_key(line: &str) -> Option<String> {
        let (key, _value) = line.split_once(':')?;
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return None;
        }
        Some(key.to_string())
    }

    /// The string values of an exported array/set literal, read out of the panel's source.
    ///
    /// The bound is the first `[`…`]` after the declaration, so a constant written as
    /// `= new Set<string>([ ... ])` reads the same way as `= [ ... ]`. `why` exists because a
    /// panic that says "unwrap failed" cannot be told apart from the defect the check exists
    /// to find, and a check that fails for a reason the defect does not cause trains its
    /// reader to ignore it.
    fn read_string_set(
        panel: &str,
        declaration: &str,
        why: &str,
    ) -> std::collections::BTreeSet<String> {
        let start = panel
            .find(declaration)
            .unwrap_or_else(|| panic!("{why} — the panel no longer declares {declaration}"));
        let rest = &panel[start..];
        let open = rest
            .find('[')
            .unwrap_or_else(|| panic!("{why} — {declaration} is not a list literal"));
        let close = rest[open..]
            .find(']')
            .unwrap_or_else(|| panic!("{why} — {declaration} has an unterminated list"));
        rest[open + 1..open + close]
            .split(',')
            .filter_map(|value| {
                let value = value.trim();
                value
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .map(str::to_string)
            })
            .collect()
    }

    fn read_panel(relative: &str) -> String {
        let path = format!("{}/../../apps/admin/{relative}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!("cannot read {relative} ({error}); the closed lists are duplicated in it")
        })
    }

    /// The lists and the migrations' check constraints are the same lists, written twice.
    ///
    /// Each migration is read from the repository rather than pasted here, so the assertion
    /// cannot itself drift out of date — and each is checked against the migration that
    /// *owns* its constraint, because the assignment targets live in the slice-2 file and
    /// asserting them against the slice-1 file would pass for a week after the constraint
    /// was deleted from the database.
    #[test]
    fn the_migration_agrees_with_the_lists() {
        let slice1 = read_migration("0055_crm_lead_intake.sql");
        let slice2 = read_migration("0056_crm_assignment_sla.sql");

        for (sql, constraint, list) in [
            (&slice1, "crm_leads_status_check", &STATUSES[..]),
            (&slice1, "crm_leads_decision_check", &DECISIONS[..]),
            (&slice1, "crm_intake_sources_kind_check", &SOURCE_KINDS[..]),
            (
                &slice1,
                "crm_intake_sources_dedupe_policy_check",
                &DEDUPE_POLICIES[..],
            ),
            (
                &slice2,
                "crm_assignment_rules_target_check",
                &ASSIGNMENT_TARGETS[..],
            ),
        ] {
            let needle = quoted_list(list);
            assert!(
                sql.contains(&needle),
                "{constraint} must list exactly {needle} — a value the crate accepts and the \
                 database refuses (or the reverse) is a filter that silently returns nothing"
            );
        }
    }

    fn read_migration(name: &str) -> String {
        let path = format!(
            "{}/../../database/migrations/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!("cannot read {name} ({error}); the closed lists are duplicated in it")
        })
    }

    /// `('a', 'b', 'c')` — how the migration writes its check-constraint values, except for
    /// the decision constraint where the first five are `null or …`. The needle below is the
    /// same list either way, which is why the helper is shared.
    fn quoted_list(values: &[&str]) -> String {
        let inner: Vec<String> = values.iter().map(|value| format!("'{value}'")).collect();
        format!("({})", inner.join(", "))
    }
}
