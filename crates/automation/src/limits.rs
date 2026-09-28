//! The two bounds that make a rule safe to leave armed: the rate window and the
//! concurrency policy (REQ-003 slice 4).
//!
//! Both are answered by the **same write that starts the run**. That is the whole design,
//! and the request's own risk note says why: *"rate limiting and concurrency must be
//! enforced in the same transaction that starts a run, or two API instances both pass the
//! check"*. A guard that reads the counter, decides, and then writes the run is three
//! statements; two API instances interleaving them both see `0` and both start a run, and
//! the guard has proven nothing except that it existed. Here the window row is locked
//! `for update` and the run is created on the same connection, so the two facts are one
//! fact: either the run exists and its count is included, or neither.
//!
//! ## Why the window row is created before it is locked
//!
//! [`admit`] does `insert … on conflict do nothing` and *then* `select … for update`. A
//! guard that had to `insert` the row it was about to lock would have to choose between
//! locking nothing (two callers both insert, one fails, and a first call is a 500) and
//! locking a row that might not exist. Creating it first — and letting a concurrent
//! creator's `on conflict do nothing` be a no-op — means the lock is always on a row that
//! is already there. The cost is one extra statement per *first* run of a rule, which is
//! the cheapest place to pay it.
//!
//! ## The rolling hour
//!
//! The window is `(window_start, run_count)` and it is a **rolling** hour, not a fixed
//! bucket: a rule that ran 60 times at 10:59 is allowed to start again at 11:01. That is
//! why the comparison is `now() - window_start >= 1 hour` rather than a bucket index. It
//! is the same shape as the inbound hook's own window (`crate::hooks::count_hit`) on
//! purpose: an operator who has learned to read one of them can read the other.
//!
//! ## What the caller does with a refusal
//!
//! Nothing. Both refusals are *audits and reports*, not errors: a rate-limited or skipped
//! trigger is an ordinary fact about a busy rule, and a `matcher::drain` that returned
//! `Err` on it would take the cursor transaction down with it and re-evaluate every event
//! in the batch. [`Admit`] carries the verdict, the caller records it, and the drain
//! carries on. The one thing a refusal must never do is *hide* itself, which is why
//! `workflows.last_error` is written in the same transaction as the counter and is
//! cleared when a run is admitted again.

use serde_json::json;
use sqlx::{PgConnection, PgPool};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::error::Result;

/// Lowest and highest `rate_limit_per_hour` the panel and the database both accept.
///
/// The bounds are a pair: `0` would mean "never run", which is what the `enabled` flag is
/// for, and a number in the hundreds of thousands is not a rate limit but a throughput
/// promise. The check lives in the database too (migration `0029`), so a row written by
/// hand is bounded the same way a row written by the panel is.
pub const MIN_RATE_LIMIT: i32 = 1;

/// Highest `rate_limit_per_hour` a rule may carry.
pub const MAX_RATE_LIMIT: i32 = 10_000;

/// Default for a rule created without one: an hour of ordinary traffic, not an hour of
/// a signup storm.
pub const DEFAULT_RATE_LIMIT: i32 = 60;

/// Length of the rolling window. One hour, named once.
pub const WINDOW: Duration = Duration::hours(1);

/// What a second trigger does while a run of the same rule is still going.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Concurrency {
    /// The new run waits: the rule's runs take turns.
    Queue,
    /// The new trigger is dropped and reported; the run that is going finishes alone.
    Skip,
}

impl Concurrency {
    /// Canonical lowercase name stored in the database.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queue => "queue",
            Self::Skip => "skip",
        }
    }

    /// Parse a stored value; `None` for anything else, so an unreadable column falls back
    /// to the safer of the two in [`Concurrency::parse_or_default`] rather than guessing.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "queue" => Some(Self::Queue),
            "skip" => Some(Self::Skip),
            _ => None,
        }
    }

    /// The closed set, for the panel's picker — it offers both, so the vocabulary is
    /// read from the layer rather than written into a `select`.
    pub const ALL: &'static [Concurrency] = &[Concurrency::Queue, Concurrency::Skip];

    /// Parse a stored value, defaulting to `queue` for anything unreadable.
    ///
    /// The default is `queue` and not `skip` because `queue` is the behaviour a rule had
    /// before the policy existed, and because dropping a run is the destructive answer:
    /// an unreadable policy must not be the reason a rule stopped sending.
    #[must_use]
    pub fn parse_or_default(raw: &str) -> Self {
        Self::parse(raw).unwrap_or(Self::Queue)
    }

    /// What this policy means, in a sentence the panel shows next to the picker.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Queue => {
                "Triggers that arrive while a run is going wait their turn, one run at a time."
            }
            Self::Skip => {
                "A trigger that arrives while a run is going is dropped and reported, so the run \
                 that is already working finishes alone."
            }
        }
    }
}

/// A rule's own rate limit and concurrency policy, as read from its row.
///
/// Kept apart from the engine's `Workflow` on purpose: this is a *bound*, not a
/// definition, and the matcher needs it before it has decided to materialise anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// Runs allowed per rolling hour.
    pub rate_limit_per_hour: i32,
    /// What a concurrent trigger does.
    pub concurrency: Concurrency,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            rate_limit_per_hour: DEFAULT_RATE_LIMIT,
            concurrency: Concurrency::Queue,
        }
    }
}

impl Policy {
    /// Read a policy out of two stored columns, clamping and defaulting.
    ///
    /// Clamping rather than refusing is the right answer *here* and only here: the
    /// database already refuses an out-of-range value on the way in, so an out-of-range
    /// value at this point is either a hand-written row or a hand-edited one, and a run
    /// that has already been authorised should not fail on a bad integer. The panel's
    /// own save path refuses it in words.
    #[must_use]
    pub fn from_columns(rate_limit_per_hour: i32, concurrency: &str) -> Self {
        Self {
            rate_limit_per_hour: rate_limit_per_hour.clamp(MIN_RATE_LIMIT, MAX_RATE_LIMIT),
            concurrency: Concurrency::parse_or_default(concurrency),
        }
    }

    /// Check a rate limit the panel sent.
    pub fn check_rate_limit(raw: i32) -> Result<i32> {
        if !(MIN_RATE_LIMIT..=MAX_RATE_LIMIT).contains(&raw) {
            return Err(crate::error::AutomationError::invalid(
                "invalid_rate_limit",
                format!(
                    "a rate limit is between {MIN_RATE_LIMIT} and {MAX_RATE_LIMIT} runs an hour; \
                     use the rule's switch instead of a limit of 0 to stop it firing"
                ),
            ));
        }
        Ok(raw)
    }
}

/// What the guard decided about one trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    /// The run may start. `used` is the count *including* this one, so the panel's
    /// "7 of 60" is the number the database will agree with.
    Allowed {
        /// Runs counted in this window, including this one.
        used: i32,
        /// The rule's ceiling.
        limit: i32,
    },
    /// The rule has spent its hour.
    RateLimited {
        /// Runs already counted in this window.
        used: i32,
        /// The rule's ceiling.
        limit: i32,
        /// When the window rolls over.
        resets_at: OffsetDateTime,
    },
    /// A run of this rule is already going and the policy is `skip`.
    Skipped {
        /// The run that holds the slot.
        running: Uuid,
    },
}

impl Admit {
    /// `true` when a run may start.
    #[must_use]
    pub const fn is_allowed(self) -> bool {
        matches!(self, Self::Allowed { .. })
    }

    /// The `workflow.step` / `automation.rule.limit_reached` payload for a refusal.
    #[must_use]
    pub fn reason(self, policy: &Policy) -> Option<String> {
        match self {
            Self::Allowed { .. } => None,
            Self::RateLimited {
                used, limit, ..
            } => Some(format!(
                "the rule may start {limit} runs an hour and has started {used} in this window; \
                 this trigger was not run and the next one may start after the window rolls over"
            )),
            Self::Skipped { running } => {
                let _ = policy;
                Some(format!(
                    "a run of this rule ({running}) was still going, so this trigger was skipped \
                     rather than queued"
                ))
            }
        }
    }
}

/// The current window of a rule, without taking the lock.
///
/// For the panel's "7 of 60 in this hour" line and nothing else: a read that is a few
/// seconds stale is a counter, and the *decision* is [`admit`], which locks. Reading the
/// lock-held value from here is exactly the mistake the design refuses to make twice.
pub async fn window_state(pool: &PgPool, workflow_id: Uuid) -> Result<(i32, OffsetDateTime)> {
    let state: Option<(OffsetDateTime, i32)> = sqlx::query_as(
        "select window_start, run_count from workflow_rate_windows where workflow_id = $1",
    )
    .bind(workflow_id)
    .fetch_optional(pool)
    .await?;

    Ok(state
        .map(|(start, count)| (count, start))
        .unwrap_or((0, OffsetDateTime::now_utc())))
}

/// The number of this rule's runs that are still open, and the oldest of them.
///
/// "Open" is `running` and `awaiting_approval`: a run parked on a person has *not*
/// released its slot, and a rule that queues behind it is exactly the behaviour
/// `concurrency = queue` promises. A run parked on a gate is the reason that policy
/// needs a `skip`: a gate nobody is watching is a rule that stops doing anything at all.
pub async fn running_executions(
    connection: &mut PgConnection,
    workflow_id: Uuid,
) -> Result<Option<(Uuid, OffsetDateTime)>> {
    let holder: Option<(Uuid, OffsetDateTime)> = sqlx::query_as(
        "select id, started_at from workflow_executions \
         where workflow_id = $1 and status in ('running', 'awaiting_approval') \
         order by started_at asc, id limit 1",
    )
    .bind(workflow_id)
    .fetch_optional(connection)
    .await?;

    Ok(holder)
}

/// Decide whether a trigger may start a run, and count it if so.
///
/// **Must be called inside the same transaction that creates the execution.** The window
/// row is created if it is missing and then locked, and the counter is bumped *before* the
/// caller creates the run — if the caller then fails to create it, the whole transaction
/// rolls back and the count with it. That is the property that makes the counter a fact
/// about runs rather than a count of attempts.
pub async fn admit(
    connection: &mut PgConnection,
    workflow_id: Uuid,
    policy: &Policy,
    now: OffsetDateTime,
) -> Result<Admit> {
    // The row first, so the lock below is always on a row that exists. `do nothing` makes
    // a concurrent creator a no-op rather than an error: two rules' first runs in the same
    // millisecond must not be a unique-violation.
    sqlx::query(
        "insert into workflow_rate_windows (workflow_id, window_start, run_count) \
         values ($1, $2, 0) on conflict (workflow_id) do nothing",
    )
    .bind(workflow_id)
    .bind(now)
    .execute(&mut *connection)
    .await?;

    // …and the lock. Everything from here is serialised per rule, which is what stops two
    // API instances from both reading the same count.
    let window: (OffsetDateTime, i32) = sqlx::query_as(
        "select window_start, run_count from workflow_rate_windows \
         where workflow_id = $1 for update",
    )
    .bind(workflow_id)
    .fetch_one(&mut *connection)
    .await?;

    // A window older than an hour is a fresh allowance; the old count goes with it.
    let (start, used) = if now - window.0 >= WINDOW {
        (now, 0)
    } else {
        (window.0, window.1)
    };

    if used >= policy.rate_limit_per_hour {
        // The refusal writes the window's own row and the rule's last error, and nothing
        // else: no run, no step, no counter change. The audit row that follows the commit
        // is the record of it.
        sqlx::query(
            "update workflow_rate_windows set window_start = $2, run_count = $3 \
             where workflow_id = $1",
        )
        .bind(workflow_id)
        .bind(start)
        .bind(used)
        .execute(&mut *connection)
        .await?;
        set_last_error(
            connection,
            workflow_id,
            &format!(
                "the hour's {} run(s) are already spent; triggers are being refused",
                policy.rate_limit_per_hour
            ),
        )
        .await?;

        return Ok(Admit::RateLimited {
            used,
            limit: policy.rate_limit_per_hour,
            resets_at: start + WINDOW,
        });
    }

    // The concurrency check runs *after* the rate check and inside the same lock, so a
    // skipped trigger is not also counted against the hour. Counting a run that was never
    // started would make the window a count of *triggers* rather than of runs, and the
    // two answer different questions: "how often may this rule fire" and "how many things
    // is it doing at once".
    if policy.concurrency == Concurrency::Skip {
        if let Some((running, _started)) = running_executions(connection, workflow_id).await? {
            return Ok(Admit::Skipped { running });
        }
    }

    let used = used + 1;
    sqlx::query(
        "update workflow_rate_windows set window_start = $2, run_count = $3 where workflow_id = $1",
    )
    .bind(workflow_id)
    .bind(start)
    .bind(used)
    .execute(&mut *connection)
    .await?;

    // An admitted run clears the line the last refusal wrote, so the rule stops showing a
    // stale error the moment it starts working again. Clearing it here rather than at the
    // end of the run is deliberate: what the panel shows is "the last thing that went
    // wrong", and a run that *started* is a better answer than a run that finished.
    clear_last_error(connection, workflow_id).await?;

    Ok(Admit::Allowed {
        used,
        limit: policy.rate_limit_per_hour,
    })
}

/// Write the one line the rule's operations surface shows about its last refusal.
pub async fn set_last_error(
    connection: &mut PgConnection,
    workflow_id: Uuid,
    message: &str,
) -> Result<()> {
    sqlx::query("update workflows set last_error = $2, updated_at = now() where id = $1")
        .bind(workflow_id)
        .bind(truncate(message))
        .execute(&mut *connection)
        .await?;
    Ok(())
}

/// Clear the last refusal, which is what admitting a run does.
pub async fn clear_last_error(connection: &mut PgConnection, workflow_id: Uuid) -> Result<()> {
    sqlx::query(
        "update workflows set last_error = null where id = $1 and last_error is not null",
    )
    .bind(workflow_id)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

/// Cut a message to what the column (and the panel's line) can hold.
///
/// The database refuses anything over 400 characters, and a refusal message built from a
/// count and a policy is never near that — but the bound is enforced here rather than
/// discovered as a constraint violation on a live run, because a *run* is the worst place
/// to find out that a message is too long.
fn truncate(message: &str) -> &str {
    match message.char_indices().nth(400) {
        Some((at, _)) => &message[..at],
        None => message,
    }
}

/// The audit metadata of a refusal, for `automation.rule.limit_reached`.
///
/// The bound that produced it is named, not just its numbers: an operator reading a
/// run history needs to know *which* bound refused the run to know which control to
/// change, and `used`/`limit` alone reads as a metric.
#[must_use]
pub fn limit_metadata(verdict: Admit, policy: &Policy) -> Option<serde_json::Value> {
    let reason = verdict.reason(policy)?;
    Some(match verdict {
        Admit::Allowed { .. } => unreachable!("an admitted run has no reason"),
        Admit::RateLimited {
            used, limit, ..
        } => json!({
            "bound": "rate_limit_per_hour",
            "used": used,
            "limit": limit,
            "concurrency": policy.concurrency.as_str(),
            "reason": reason,
        }),
        Admit::Skipped { running } => json!({
            "bound": "concurrency",
            "concurrency": policy.concurrency.as_str(),
            "running_execution_id": running,
            "reason": reason,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy(rate: i32, concurrency: Concurrency) -> Policy {
        Policy {
            rate_limit_per_hour: rate,
            concurrency,
        }
    }

    #[test]
    fn a_limit_the_panel_could_not_explain_is_refused_in_words() {
        assert_eq!(Policy::check_rate_limit(1).expect("lowest"), 1);
        assert_eq!(
            Policy::check_rate_limit(MAX_RATE_LIMIT).expect("highest"),
            MAX_RATE_LIMIT
        );
        for bad in [0, -1, MAX_RATE_LIMIT + 1, i32::MIN, i32::MAX] {
            let error = Policy::check_rate_limit(bad).expect_err("out of range");
            assert_eq!(error.code(), "invalid_rate_limit", "{bad}");
            // The message has to name the switch, because "0" is what an author reaches
            // for when they mean "off" and it is a limit, not a switch.
            assert!(error.to_string().contains("switch"), "{bad}");
        }
    }

    #[test]
    fn an_unreadable_policy_falls_back_to_the_behaviour_the_rule_already_had() {
        // Queue is what a rule did before the policy existed, and dropping a run is the
        // destructive answer: a corrupt column must not be the reason a rule goes quiet.
        assert_eq!(
            Concurrency::parse_or_default("something-else"),
            Concurrency::Queue
        );
        assert_eq!(Concurrency::parse_or_default(""), Concurrency::Queue);
        assert_eq!(Concurrency::parse("skip"), Some(Concurrency::Skip));
        assert_eq!(Concurrency::parse("queue"), Some(Concurrency::Queue));
        assert_eq!(Concurrency::Skip.as_str(), "skip");

        for policy in Concurrency::ALL {
            assert!(!policy.describe().is_empty(), "the picker shows the sentence");
        }
    }

    #[test]
    fn a_stored_policy_is_clamped_rather_than_trusted() {
        // The database already refuses an out-of-range value on the way in, so a value
        // that arrives out of range was written by hand — and a run that has been
        // authorised should not fail on a bad integer.
        assert_eq!(
            Policy::from_columns(0, "skip").rate_limit_per_hour,
            MIN_RATE_LIMIT
        );
        assert_eq!(
            Policy::from_columns(999_999, "skip").rate_limit_per_hour,
            MAX_RATE_LIMIT
        );
        assert_eq!(
            Policy::from_columns(30, "skip").concurrency,
            Concurrency::Skip
        );
    }

    #[test]
    fn a_refusal_says_which_bound_produced_it() {
        let queued = policy(60, Concurrency::Queue);
        let limited = Admit::RateLimited {
            used: 60,
            limit: 60,
            resets_at: OffsetDateTime::now_utc(),
        };
        let metadata = limit_metadata(limited, &queued).expect("a refusal has a reason");
        assert_eq!(metadata["bound"], "rate_limit_per_hour");
        assert_eq!(metadata["used"], 60);
        assert_eq!(metadata["limit"], 60);
        assert!(
            metadata["reason"]
                .as_str()
                .expect("a sentence")
                .contains("not run"),
            "the reason has to say the trigger did not happen, not just that a number was high"
        );

        let skipped = Admit::Skipped {
            running: Uuid::nil(),
        };
        let metadata = limit_metadata(skipped, &policy(60, Concurrency::Skip)).expect("a reason");
        assert_eq!(metadata["bound"], "concurrency");
        assert_eq!(metadata["concurrency"], "skip");
        assert_eq!(
            metadata["running_execution_id"],
            json!(Uuid::nil()),
            "the running execution id is carried as a string, the way a uuid reads inside a payload"
        );

        // An admitted run has no refusal to report, so it produces no event at all.
        assert!(limit_metadata(Admit::Allowed { used: 1, limit: 60 }, &queued).is_none());
        assert!(Admit::Allowed { used: 1, limit: 60 }.is_allowed());
        assert!(!limited.is_allowed());
    }

    #[test]
    fn a_message_too_long_for_the_column_is_cut_rather_than_failing_a_run() {
        // The column refuses over 400 characters, and finding that out on a live run is
        // the worst possible place to find it out.
        let long = "x".repeat(4_000);
        let cut = truncate(&long);
        assert_eq!(cut.chars().count(), 400);
        assert!(truncate("short").len() < 400);
    }
}
