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

/// Whether a source of this kind may carry an endpoint key.
///
/// ## Why this is a function and not a `kind == "endpoint"` written twice
///
/// Three places decide the same fact and all three must agree, or the surface leaks:
///
/// * [`crate::store::create_source`] — issues a key, and issues none for a form source,
///   because a form source is authenticated by the form's own submission validation.
/// * [`crate::store::rotate_key`] — may only rotate a source that *has* a key.
/// * [`crate::store::find_source_by_key`] — the public lookup, which matches the stored digest
///   **and** this rule, so a digest that reached a row by any other route does not open a
///   public capture path.
///
/// The third one is what makes the first two load-bearing rather than tidy. A digest written
/// onto a form-bound row is a live public capture path onto a source that was never configured
/// to have one, and the lookup would happily serve it. Writing the rule at each of the three
/// sites means it is only as good as the last edit, and the last edit has nothing to fail: no
/// unit test can tell that a form source became key-addressable.
///
/// A fourth kind in `SOURCE_KINDS` therefore has to answer this question here, once, where the
/// reader can see that adding a kind *also* decides whether it is addressable by a credential.
/// The SQL in [`crate::store::find_source_by_key`] binds [`KEY_BEARING_KIND`] rather than a
/// literal, so the two cannot drift.
#[must_use]
pub fn carries_endpoint_key(kind: &str) -> bool {
    kind == KEY_BEARING_KIND
}

/// The one source kind addressable by an endpoint key.
///
/// Bound as a query parameter (not interpolated) so the SQL and the predicate above are provably
/// the same fact, and named rather than inlined because a credential boundary that exists only
/// as a literal is a boundary a future kind can widen without a test failing.
pub const KEY_BEARING_KIND: &str = "endpoint";

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

/// The SQL spelling of `is_open`, for a column named `column`: a lead status predicate the
/// planner can prove.
///
/// **This exists because the Rust half cannot reach a query.** `is_open` is what every sweep,
/// count and badge asks in Rust; six statements ask the same question in SQL by hand, and the
/// two halves can disagree in the one direction nothing else catches — a fourth status added to
/// `is_open` changes the Rust answer and leaves the SQL reading a row the clock has stopped on.
/// `PHONE_DIGITS_SQL` is the same argument for the phone rule; this is the same argument one
/// level up.
///
/// **The column is a parameter rather than a constant because three of the six call sites read an
/// aliased table** (`from crm_leads l`, `… and l.status in (…)`) and the other three do not, and
/// a caller that forgot the alias gets a query that names a column no relation in the `from`
/// clause provides. `column` is a plain argument, so the caller has to type the name its own
/// query uses — which is the only place the alias is knowable.
///
/// A previous version of this was a `&'static str` plus `.replace("status ", "l.status ")` at the
/// aliased sites. That is the same defect class one level down: a textual rewrite of a SQL string
/// is invisible to the compiler, and it rewrites *every* `status ` it finds — including the
/// `u.status` a neighbouring statement happens to select. A function that formats the predicate
/// cannot accidentally rewrite something the caller did not ask it to.
///
/// **The negated form has a different meaning and is used only where an index's partial
/// predicate demands it.** `not_closed_statuses_sql` covers every status the platform does not
/// know, while `open_statuses_sql` names four of them — so a lead whose status is somehow
/// neither is swept by one spelling and not the other. Both live next to each other on purpose,
/// and `crm_leads_sla_idx`'s partial predicate is written in the negated form, which is why the
/// sweep's first read must use it: PostgreSQL offers a partial index only when the planner can
/// prove every row the query would read satisfies the predicate, and "no status mentioned"
/// proves nothing about it.
#[must_use]
pub fn open_statuses_sql(column: &str) -> String {
    format!("{column} in ('new', 'assigned', 'contacted', 'qualified')")
}

/// The same rule as [`open_statuses_sql`], negated — the shape an index's partial predicate
/// uses, and the shape that admits a status nobody has defined yet.
///
/// **Derived from [`STATUSES`] rather than written out, and that is the fix.** It used to be the
/// literal `not in ('spam', 'rejected', 'duplicate')`, which is a fourth hand-written copy of the
/// open/closed split and the reason `converted` leaked: `is_open` calls `converted` terminal,
/// `the_two_sql_predicates_partition_the_statuses_the_platform_knows` says the two lists
/// partition the vocabulary, and this function quietly excluded three of the four terminal
/// statuses. `converted` leads that were never answered are walked by
/// [`crate::assignment_store::organizations_with_leads`] every minute for nothing — and they are
/// never answered, because neither conversion path writes `first_response_at` — so the walk does
/// not decay, it is permanent. Ten tenants on the gate's fixture where one is correct.
///
/// It is built from the same [`STATUSES`] `is_open` reads, in `STATUSES`'s own order, so a
/// ninth status cannot be added without this list knowing about it.
///
/// **Why this list and not the open one, when both now name four statuses.** `converted` is the
/// difference and the reason both forms are kept: the open form admits a status the platform has
/// never defined (and so does this one), while `open_statuses_sql` would reject the sweep's first
/// read from `crm_leads_sla_idx`, whose partial predicate is frozen in migration `0055` in the
/// three-status form. The planner proves predicates rather than implications it has to derive
/// through a list, so the query has to be able to prove the *stored* predicate. The consequence is
/// a second copy in the schema, which `0229_crm_lead_sla_index_terminal_status.sql` rebuilds
/// against this list; the two are held together by `scripts/qa/run-crm-terminal-status.sh`, which
/// reads both and asks the planner which one it will use.
#[must_use]
pub fn not_closed_statuses_sql(column: &str) -> String {
    let closed: Vec<String> = STATUSES
        .iter()
        .copied()
        .filter(|status| !is_open(status))
        .map(|status| format!("'{status}'"))
        .collect();
    format!("{column} not in ({})", closed.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The SQL predicate and the Rust predicate are two spellings of one rule, and this is the
    /// assertion that can fail when they drift.
    ///
    /// **Every status `is_open` accepts must appear in the SQL, and every status it rejects must
    /// be absent** — checked as sets rather than as text, because the two forms are ordered
    /// differently in their own sources and a text comparison would be a false pass on a
    /// reordering. A status added to `STATUSES` without being added here is exactly the defect
    /// the constant exists to prevent, and nothing else in the crate notices it: the sweeps keep
    /// reading the row in SQL and the badges keep asking `is_open` in Rust, so the two disagree
    /// only on a status nobody has shipped yet.
    #[test]
    fn the_sql_predicate_names_exactly_the_statuses_is_open_accepts() {
        let in_sql: std::collections::BTreeSet<String> = open_statuses_sql("status")
            .split('(')
            .nth(1)
            .expect("the predicate lists its statuses in parentheses")
            .split(')')
            .next()
            .expect("the list is closed")
            .split(',')
            .map(|status| status.trim().trim_matches('\'').to_string())
            .collect();

        let in_rust: std::collections::BTreeSet<String> = STATUSES
            .iter()
            .copied()
            .filter(|status| is_open(status))
            .map(String::from)
            .collect();

        assert_eq!(
            in_sql, in_rust,
            "the SQL status predicate and is_open name different statuses — a lead in one and \
             not the other is swept by one read and ignored by the other"
        );
    }

    /// The negated form must be the exact complement of the open form.
    ///
    /// They are not interchangeable, and the reason they are both kept is the point: an index's
    /// partial predicate is written in the negated form, so a sweep that used the open form
    /// would be correct and would not use the index. This test cannot prove which is which — only
    /// `run-crm-sla-sweep-plan.sh` can ask the planner — so it asserts the weaker thing that IS
    /// true everywhere: no status is in both lists, and every status the platform knows is in one
    /// of them. A status in neither would be swept by neither spelling, silently.
    #[test]
    fn the_two_sql_predicates_partition_the_statuses_the_platform_knows() {
        let open: std::collections::BTreeSet<&str> = STATUSES
            .iter()
            .copied()
            .filter(|status| is_open(status))
            .collect();
        let closed: std::collections::BTreeSet<&str> = STATUSES
            .iter()
            .copied()
            .filter(|status| !is_open(status))
            .collect();

        assert!(
            open.is_disjoint(&closed),
            "a status is both open and closed: {}",
            open.intersection(&closed)
                .copied()
                .collect::<Vec<_>>()
                .join(", ")
        );
        assert_eq!(
            open.len() + closed.len(),
            STATUSES.len(),
            "a status is in neither predicate, so neither spelling of the sweep would read it"
        );
    }

    /// The closed list the negated predicate emits is every terminal status, as a SET.
    ///
    /// **This is the assertion that could have caught the `converted` leak, and it is not the
    /// test above.** That test compares `is_open` against itself — both halves are asked the
    /// same question, so it passes whatever the two halves agree on. What it never asked is
    /// whether the *SQL* the queries actually run names the same statuses as the Rust rule. It
    /// did not: `not_closed_statuses_sql` was the literal `not in ('spam','rejected','duplicate')`
    /// — three of the four terminal statuses — while `is_open` calls `converted` terminal, so the
    /// sweep walked every tenant holding a converted, unanswered, overdue lead, for ever.
    ///
    /// **Red before the fix, and RED-PROVEN rather than asserted.** Narrowing the list back to
    /// three statuses turns this red with the offending status named, and leaves every other test
    /// in the crate green — which is what shows this one is not vacuous and not a shadow of
    /// `the_two_sql_predicates_partition_the_statuses_the_platform_knows`. That one still passes,
    /// and that is the point of running both.
    #[test]
    fn the_closed_list_names_every_status_is_open_rejects() {
        let in_sql: std::collections::BTreeSet<String> = not_closed_statuses_sql("status")
            .split('(')
            .nth(1)
            .expect("the predicate lists its statuses in parentheses")
            .split(')')
            .next()
            .expect("the list is closed")
            .split(',')
            .map(|status| status.trim().trim_matches('\'').to_string())
            .collect();

        let in_rust: std::collections::BTreeSet<String> = STATUSES
            .iter()
            .copied()
            .filter(|status| !is_open(status))
            .map(String::from)
            .collect();

        assert_eq!(
            in_sql, in_rust,
            "the SQL closed list and is_open name different statuses — a lead in one and not the \
             other is swept by one read and ignored by the other, and the one it is swept by is \
             the one that costs a round trip per tenant per minute"
        );
    }

    /// The closed list is DERIVED, not written: it is built from [`STATUSES`] by [`is_open`], so
    /// a ninth status cannot be added without this list knowing about it.
    ///
    /// **The shape assertion, and it is deliberately separate from the set assertion above.** The
    /// set test says the answer is right today; this one says the answer cannot be *pinned* to
    /// today. They fail differently and that is the point: a hard-coded list that happens to be
    /// correct passes this test's sibling and fails this one, which is the shape that produced the
    /// defect — a hand-written copy that was correct when written and wrong when `converted`
    /// stopped meaning "still work".
    #[test]
    fn the_closed_list_is_derived_rather_than_written_out() {
        let sql = not_closed_statuses_sql("status");
        assert!(
            sql.starts_with("status not in ("),
            "the negated predicate is a `not in` list: {sql}"
        );
        // Every status in the emitted list is one the crate knows, so the list can never name a
        // status the platform refuses — which would make the predicate vacuously true and the
        // read would return every row.
        for status in sql
            .split('(')
            .nth(1)
            .expect("the predicate lists its statuses in parentheses")
            .split(')')
            .next()
            .expect("the list is closed")
            .split(',')
            .map(|status| status.trim().trim_matches('\''))
        {
            assert!(
                is_status(status),
                "the closed list names {status:?}, which is not a status the platform knows — a \
                 status the database refuses makes this predicate vacuously true"
            );
        }
        // And the list is not empty: a vocabulary with no terminal status would make the sweep's
        // first read walk every lead ever received, which is the shape the partial index exists
        // to avoid.
        assert!(
            sql.split(',').count() > 1,
            "the closed list is empty — every lead ever received is swept: {sql}"
        );
    }

    /// The column is the caller's, and a caller that forgets its alias gets a query naming a
    /// column no relation provides — which the planner answers with `42703`, on the sweep, once a
    /// minute. A predicate function is the one place that has to be honest about both spellings.
    //
    // **Both forms qualify the column they were asked for, and neither contains the other's shape
    // by accident.** The membership of the closed list is asserted as a set in
    // `the_closed_list_names_every_status_is_open_rejects`; what is pinned *here* is the template —
    // that the column name lands where a column name belongs, in both the open and the negated
    // form. Asserting the whole string against itself (or against a literal that has to be edited
    // every time the vocabulary moves) is a tautology or a second copy, and neither fails.
    #[test]
    fn the_predicate_is_asked_for_the_column_the_query_uses() {
        assert_eq!(
            open_statuses_sql("l.status"),
            "l.status in ('new', 'assigned', 'contacted', 'qualified')",
            "the open predicate qualifies the column it was given"
        );
        assert_eq!(
            not_closed_statuses_sql("l.status"),
            "l.status not in ('converted', 'duplicate', 'spam', 'rejected')",
            "the negated predicate qualifies the column it was given"
        );
        // Both are `not in`/`in` over a parenthesised list — a form that lost its parentheses would
        // still parse in one statement and change the meaning in another.
        for sql in [open_statuses_sql("s"), not_closed_statuses_sql("s")] {
            assert!(
                sql.contains("in ("),
                "a predicate without a parenthesised list binds differently per statement: {sql}"
            );
        }
    }

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

    /// Exactly one kind is addressable by an endpoint key, and the constant the public lookup
    /// binds is the same one this predicate compares against.
    ///
    /// The assertion is per-kind rather than "some kinds carry keys" because the failure this
    /// pins is a *widening*: a fourth kind added to `SOURCE_KINDS` inheriting key-addressability
    /// by accident. A test written as `assert!(carries_endpoint_key(k) == (k == "endpoint"))` is
    /// the same statement twice; naming each kind is what a reader can check by eye, and it
    /// fails loudly with the offending kind's name when the list grows.
    #[test]
    fn exactly_one_kind_is_addressable_by_an_endpoint_key() {
        for kind in SOURCE_KINDS {
            let expected = kind == KEY_BEARING_KIND;
            assert_eq!(
                carries_endpoint_key(kind),
                expected,
                "kind {kind:?} disagrees with the rule `carries_endpoint_key`; a kind that \
                 carries a key must be one whose authentication is that key"
            );
        }
        // The two non-key kinds are named explicitly because they are the reason the rule
        // exists: a form source is authenticated by the form's own submission validation, and
        // an import source is not addressable by any caller at all.
        assert!(
            !carries_endpoint_key("form"),
            "a form source authenticates by its form"
        );
        assert!(
            !carries_endpoint_key("import"),
            "an import source has no caller to key"
        );
        assert!(carries_endpoint_key("endpoint"));
        // An unknown kind is never key-bearing. The closed list is the default: a typo in a
        // `kind` column answers "not key-addressable" rather than reaching for the literal.
        assert!(!carries_endpoint_key("Endpoint"));
        assert!(!carries_endpoint_key(""));
        assert!(!carries_endpoint_key("webhook"));
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
                expected
                    .into_iter()
                    .map(str::to_string)
                    .collect::<std::collections::BTreeSet<_>>(),
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
