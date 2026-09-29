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
