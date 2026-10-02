//! Print the crate's SQL status predicates, one per line, so a QA gate can use them.
//!
//! ## Why this binary exists
//!
//! The gates used to recover `open_statuses_sql` and `not_closed_statuses_sql` by regexing
//! `vocabulary.rs` for the body of the `format!` inside them. That worked while both were
//! literals and stopped working the moment `not_closed_statuses_sql` was **derived** from
//! `STATUSES` — which is precisely the change that fixed the `converted` leak, because a
//! hard-coded list is exactly what let it leak. So the reading method died with the fix.
//!
//! That is the defect class of the last two ticks applied to this tick's own tool: **a checker
//! that reads the code's text stops working when the code stops being text-shaped, and the
//! failure looks like "could not read the predicate" rather than like a broken checker.** The
//! answer is not a cleverer regex; it is to ask the code for its answer. `cargo test` cannot do
//! it (the value never surfaces), so this example does, and the gate runs it.
//!
//! Output is one predicate per line, prefixed by its function name, so a gate cannot pick up a
//! line belonging to the other one:
//!
//! ```text
//! open=status in ('new', 'assigned', 'contacted', 'qualified')
//! not_closed=status not in ('converted', 'duplicate', 'spam', 'rejected')
//! ```
//!
//! A gate that cannot run this must not guess the predicate from the source: a hand-recovered
//! list is a second copy of the rule, which is the whole thing this example exists to delete.

fn main() {
    println!(
        "open={}",
        omnion_module_crm_intake::vocabulary::open_statuses_sql("status")
    );
    println!(
        "not_closed={}",
        omnion_module_crm_intake::vocabulary::not_closed_statuses_sql("status")
    );
}
