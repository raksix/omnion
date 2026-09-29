//! The notification vocabulary: the closed lists, and the guards over them.
//!
//! Every one of these lists exists because the alternative is a `text` column that accepts
//! anything, and a table of "categories" nobody can enumerate is a table nobody can build a
//! filter out of. The cost is a branch here for every value a future module wants; the benefit
//! is that the panel's category filter, the preference matrix and the SQL check constraint are
//! all reading the *same* list, so a category can never exist in the filter and not in the
//! database.
//!
//! **These are compile-time lists, not database rows.** That is deliberate: a module that
//! wants its own kind registers it in the router (slice 3) rather than growing a migration, and
//! the list stays small enough for a panel to render as a matrix without asking the server
//! what it should be.

/// Every category a notification may carry.
///
/// The order is the order the bell panel renders its grouped lines in, and the order the
/// preference matrix uses as its rows — so the two cannot disagree about what "the fifth
/// category" is.
pub const CATEGORIES: [&str; 6] = [
    "approval", "security", "update", "ticket", "system", "mention",
];

/// How urgent a notification is.
///
/// Ordering matters at exactly one place — the list, which sorts unread first and then by
/// priority — so the order here is the order the SQL uses.
pub const PRIORITIES: [&str; 4] = ["low", "normal", "high", "critical"];

/// How a notification can reach a person.
///
/// `in_app` is not optional and is not a preference: a notification nobody can see in the
/// panel is a row that exists only for a database, and the bell is the platform's own
/// obligation. It is in the list so the matrix can *show* it locked rather than hide it.
pub const CHANNELS: [&str; 5] = ["in_app", "email", "web_push", "webhook", "chat"];

/// The largest page of notifications one read may return.
pub const MAX_PAGE: i64 = 100;

/// The largest number of ids one bulk action may name.
pub const MAX_BULK_IDS: usize = 200;

/// How many emits one actor may make in a minute.
///
/// The emit route exists so modules can talk to a person; a module that loops would otherwise
/// be indistinguishable from a denial of service on the inbox table. Counted per *actor*, not
/// per user: two modules emitting for the same person must not be able to spend each other's
/// budget.
pub const EMIT_BUDGET_PER_MINUTE: i64 = 60;

/// `true` when `value` is a category the platform knows.
#[must_use]
pub fn is_category(value: &str) -> bool {
    CATEGORIES.contains(&value)
}

/// `true` when `value` is a priority the platform knows.
#[must_use]
pub fn is_priority(value: &str) -> bool {
    PRIORITIES.contains(&value)
}

/// `true` when `value` is a channel the platform knows.
#[must_use]
pub fn is_channel(value: &str) -> bool {
    CHANNELS.contains(&value)
}

/// The rank of a priority, for a sort that has to order by it.
///
/// **Higher is more urgent** — the rank is the position in [`PRIORITIES`], and that list is
/// written least-urgent first (`low`, `normal`, `high`, `critical`) so that the SQL's
/// `order by … desc` puts the urgent rows on top. A "lower is more urgent" convention would
/// need the list written backwards, and a vocabulary list that reads backwards is one somebody
/// eventually "fixes".
///
/// Returns [`PRIORITY_UNKNOWN`] for a value the platform does not know, which sorts *last* in
/// a descending sort — the right place for a row this build cannot classify, and not a panic:
/// an unreadable row is still a row the reader has to be able to see.
pub const PRIORITY_UNKNOWN: i32 = -1;

/// Rank of a priority string.
#[must_use]
pub fn priority_rank(value: &str) -> i32 {
    PRIORITIES
        .iter()
        .position(|candidate| *candidate == value)
        .map_or(PRIORITY_UNKNOWN, |index| index as i32)
}

/// The category a group line in the bell panel is labelled with.
///
/// A plain pass-through with one rule: an unknown category is rendered *by its own name* and
/// not dropped. A panel that silently loses a notification because a module registered a
/// category the panel has not heard of is worse than one that shows a word the reader has not
/// seen — the second is a bug report, the first is a silent omission.
#[must_use]
pub fn category_label(value: &str) -> &str {
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lists_have_no_duplicates() {
        for list in [&CATEGORIES[..], &PRIORITIES[..], &CHANNELS[..]] {
            let mut sorted = list.to_vec();
            sorted.sort_unstable();
            let before = sorted.len();
            sorted.dedup();
            assert_eq!(before, sorted.len(), "duplicate in {list:?}");
        }
    }

    #[test]
    fn the_guards_agree_with_the_lists() {
        for value in CATEGORIES {
            assert!(is_category(value));
        }
        for value in PRIORITIES {
            assert!(is_priority(value));
        }
        for value in CHANNELS {
            assert!(is_channel(value));
        }
        assert!(!is_category("invoice"));
        assert!(!is_priority("urgent"));
        assert!(!is_channel("sms"));
    }

    #[test]
    fn in_app_is_a_channel_so_the_matrix_can_lock_it() {
        // A hidden channel cannot be locked; the preference screen has to *show* it disabled.
        assert!(is_channel("in_app"));
    }

    #[test]
    fn priority_rank_orders_by_urgency_and_never_panics() {
        // Higher is more urgent (see the function's doc), so the ranking follows the list.
        assert!(priority_rank("critical") > priority_rank("low"));
        assert!(priority_rank("high") > priority_rank("normal"));
        assert!(priority_rank("critical") > priority_rank("high"));
        assert_eq!(priority_rank("normal"), 1);
        // An unknown value sorts last in a descending sort rather than panicking: an
        // unreadable row is still a row the reader has to be able to see.
        assert_eq!(priority_rank("nonsense"), PRIORITY_UNKNOWN);
        assert!(priority_rank("nonsense") < priority_rank("low"));
    }

    #[test]
    fn the_category_label_renders_what_it_was_given() {
        assert_eq!(category_label("approval"), "approval");
        assert_eq!(category_label("billing"), "billing");
    }

    /// The lists and the migration's check constraints are the same list, written twice.
    ///
    /// SQL cannot import a Rust constant, so the duplication is unavoidable — which makes it a
    /// *test* rather than a comment. The failure it prevents is the quiet one: a category
    /// added to the Rust list and not to the migration passes every unit test in the crate and
    /// then the database refuses the row in production; a category added to the migration and
    /// not to Rust is a filter the panel cannot offer. Both read as "nothing happened".
    ///
    /// The migration is read from the repository rather than pasted here, so the assertion
    /// cannot itself drift out of date.
    #[test]
    fn the_migration_agrees_with_the_lists() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../database/migrations/0050_notifications.sql"
        );
        let sql = std::fs::read_to_string(path).unwrap_or_else(|error| {
            panic!("cannot read 0050_notifications.sql ({error}); the closed lists are duplicated in it")
        });

        for (constraint, list) in [
            ("notifications_category_check", &CATEGORIES[..]),
            ("notifications_priority_check", &PRIORITIES[..]),
            ("notification_preferences_channel_check", &CHANNELS[..]),
        ] {
            let needle = quoted_list(list);
            assert!(
                sql.contains(&needle),
                "{constraint} must list exactly {needle} — a value the crate accepts and the \
                 database refuses (or the reverse) is a filter that silently returns nothing"
            );
        }
    }

    /// `('a', 'b', 'c')` — how every check constraint in the migration writes its values.
    fn quoted_list(values: &[&str]) -> String {
        let inner: Vec<String> = values.iter().map(|value| format!("'{value}'")).collect();
        format!("({})", inner.join(", "))
    }
}
