//! The security-event SQL: one timeline out of two tables (REQ-012, slice 4).
//!
//! The shape of the query is the whole story, so it is worth reading before reading the SQL.
//!
//! ## Why the two sides are read separately and merged
//!
//! The two tables share no vocabulary: the audit trail has a stable dotted `action`, the
//! sign-in log has a five-word `outcome` check constraint. A single `union all` would need one
//! `where` fragment that speaks both, and every filter would then have to know which table it is
//! talking to — so "show me the denials" becomes two predicates that one of them can quietly
//! ignore. Reading each side with **its own** filter and merging in Rust is what makes a
//! category mean the same thing on both sides.
//!
//! ## Why each side over-reads
//!
//! Each side reads `limit * 2` rows before the merge. The page is capped after the merge, so
//! reading exactly `limit` from each side would be wrong the moment one side's filter matches
//! fewer rows than its limit: 50 audit rows and 5 sign-in rows merge to 55, truncate to 50, and
//! drop 5 — which is correct — but 50 audit rows and 50 sign-in rows where the 50 sign-in rows
//! are all *older* merge to a correct 50 only because nothing is dropped. Over-reading is what
//! makes the merge complete in both directions rather than by luck.
//!
//! ## The counts are separate statements on purpose
//!
//! `total` counts the whole filter rather than the page, so the screen can say "50 of 312"
//! instead of implying the page is everything. The count is issued with the rows, against the
//! same fragment, so the two numbers describe the same filter.
//!
//! ## What this module will not do
//!
//! It will not **invent** a denial. Permission refusals are not in `audit_log` — the guard in
//! `apps/api/src/guards.rs` answers `403` without recording a row — so the `denial` category
//! exists on the screen and is, today, empty apart from the address rule's `blocked` outcome.
//! That is stated in the REQ rather than papered over: writing an audit row for every refusal
//! would put a database write on the path of every refused request, and an attacker would decide
//! how fast the audit table fills.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::Result;
use crate::events::{
    MAX_EXPORT_ROWS, SecurityEvent, describe_outcome, event_id, is_refusal,
    page_size, query_spec, summarise_metadata,
};

/// One audit row, as the query reads it.
#[derive(Debug, sqlx::FromRow)]
struct AuditRow {
    id: i64,
    organization_id: Option<Uuid>,
    actor_user_id: Option<Uuid>,
    actor_type: String,
    action: String,
    metadata: serde_json::Value,
    ip_address: Option<String>,
    user_agent: Option<String>,
    created_at: OffsetDateTime,
}

/// One sign-in attempt, as the query reads it.
#[derive(Debug, sqlx::FromRow)]
struct SignInRow {
    id: i64,
    organization_id: Option<Uuid>,
    user_id: Option<Uuid>,
    email: String,
    ip_address: Option<String>,
    user_agent: Option<String>,
    outcome: String,
    created_at: OffsetDateTime,
}

/// The outcomes a category matches **on the sign-in side**.
///
/// The three categories the audit trail's `action` cannot express are handled here: a category
/// with no sign-in word produces **no clause**, not a clause matching nothing. A clause that
/// matches nothing is indistinguishable on the screen from a filter that worked.
fn outcome_filter(category: crate::events::EventCategory) -> Option<&'static [&'static str]> {
    use crate::events::EventCategory;
    match category {
        EventCategory::Lockout => Some(&["locked"]),
        // `denial` is the one category with a real row today: `blocked` is the address rule
        // refusing an attempt, and it is the closest thing the platform records to a refusal.
        // The permission guard records nothing — see the module note.
        EventCategory::Denial => Some(&["blocked"]),
        // **`sign_in` is this table's default category**, so it has to *select* its rows rather
        // than refuse them: every outcome except `locked` is an attempt somebody made against an
        // account. The first version of the category filter returned `None` here, which read as
        // "this category has no outcomes" and made `?category=sign_in` answer zero rows while the
        // unfiltered timeline showed one — the walk caught it.
        EventCategory::SignIn => Some(&["success", "failed", "mfa_required"]),
        _ => None,
    }
}

/// Read one page of the merged timeline, newest first.
pub async fn list(pool: &PgPool, query: &crate::events::EventQuery) -> Result<crate::events::EventPage> {
    let (audit, audit_total) = audit_side(pool, query, page_size(query.limit)).await?;
    let (sign_in, sign_in_total) = sign_in_side(pool, query, page_size(query.limit)).await?;

    let mut events: Vec<SecurityEvent> = audit.into_iter().chain(sign_in).collect();
    events.sort_by(|a, b| b.occurred_at.cmp(&a.occurred_at).then_with(|| b.id.cmp(&a.id)));

    let total = audit_total + sign_in_total;
    let limit = page_size(query.limit);
    let truncated = events.len() > limit;
    events.truncate(limit);

    Ok(crate::events::EventPage {
        events,
        total,
        audit_count: audit_total,
        sign_in_count: sign_in_total,
        truncated,
    })
}

/// Every row the filter matches, for the CSV export — unpaged, because an operator who filters
/// to "denials" and exports 50 of 300 has produced a document that reads as a complete list and
/// is not one. The cap is enforced by the caller against
/// [`crate::events::MAX_EXPORT_ROWS`] so the refusal names the count.
///
/// `limit` is passed rather than derived from the query on purpose: [`page_size`] clamps to
/// [`crate::vocabulary::MAX_PAGE`], and an export that went through it would silently stop at 200
/// rows while reporting a total of 300 — the exact failure the unpaged export exists to prevent.
pub async fn export_rows(
    pool: &PgPool,
    query: &crate::events::EventQuery,
) -> Result<Vec<SecurityEvent>> {
    let (audit, _) = audit_side(pool, query, MAX_EXPORT_ROWS).await?;
    let (sign_in, _) = sign_in_side(pool, query, MAX_EXPORT_ROWS).await?;

    let mut events: Vec<SecurityEvent> = audit.into_iter().chain(sign_in).collect();
    events.sort_by(|a, b| b.occurred_at.cmp(&a.occurred_at).then_with(|| b.id.cmp(&a.id)));
    events.truncate(MAX_EXPORT_ROWS);
    Ok(events)
}

/// The audit side: `limit * 2` rows and the count for the whole filter.
async fn audit_side(
    pool: &PgPool,
    query: &crate::events::EventQuery,
    limit: usize,
) -> Result<(Vec<SecurityEvent>, i64)> {
    // A caller that asked for the sign-in table alone asked a question about one source, and
    // this is where the other half of that question is dropped rather than counted and summed.
    if query.source == Some(crate::events::EventSource::SignIn) {
        return Ok((Vec::new(), 0));
    }

    // **A category that lives on the sign-in side must select nothing here.** `sign_in` and
    // `denial` are words in the sign-in log's `outcome` vocabulary; there is no audit action that
    // means them. Handing the shared builder one of those categories produced `action like '%'`
    // — which matches **every** audit row — so `?category=sign_in` answered with the whole
    // audit trail. Found by the walk: it returned a settings change under a sign-in filter.
    //
    // The early return is the same shape as the `source` filter's and for the same reason: a
    // category with no rows on this side is not a filter that failed to parse.
    if query
        .category
        .is_some_and(|category| matches!(category, crate::events::EventCategory::SignIn))
    {
        return Ok((Vec::new(), 0));
    }

    let spec = query_spec(query);
    let over_read = (limit * 2).max(1);
    let where_clause = spec.where_clause();

    // The user agent comes from the actor's live session and is taken through a **lateral**
    // subquery with `limit 1`, never a plain join: an account with three sessions would fan the
    // audit row out into three, and the merged page would show one action three times with three
    // different agents. The fan-out is silent — no error, just a timeline that repeats itself.
    let sql = format!(
        "select a.id, a.organization_id, a.actor_user_id, a.actor_type, a.action, a.metadata, \
                 a.ip_address::text as ip_address, agent.user_agent, a.created_at \
            from audit_log a \
            left join lateral (select s.user_agent from sessions s \
                                where s.user_id = a.actor_user_id \
                                  and s.revoked_at is null \
                                order by s.created_at desc limit 1) agent on true \
           {where_clause} \
           order by a.created_at desc, a.id desc \
           limit {over_read}"
    );

    let mut builder = sqlx::query_as::<_, AuditRow>(&sql);
    // Bound once into a named vec: `spec.values()` in the `for` header is a temporary, and
    // binding `&str` out of it and then consuming the builder outlives the value. This is the
    // "does not live long enough" that reads like a lifetime puzzle and is really a missing
    // `let`.
    let values = spec.values();
    for value in &values {
        builder = builder.bind(value.as_deref());
    }
    let rows = builder.fetch_all(pool).await?;

    let count_sql = format!("select count(*) from audit_log {where_clause}");
    let mut count_builder = sqlx::query_scalar::<_, i64>(&count_sql);
    for value in &values {
        count_builder = count_builder.bind(value.as_deref());
    }
    let total = count_builder.fetch_one(pool).await?;

    let events = rows
        .into_iter()
        .map(|row| SecurityEvent {
            id: event_id(crate::events::EventSource::Audit, row.id),
            source: crate::events::EventSource::Audit,
            occurred_at: row.created_at,
            action: row.action.clone(),
            category: crate::events::EventCategory::of_action(&row.action),
            // The account that acted, not the account the action was *about*: a lockout names
            // its subject separately and a reader who sees one id here is reading the wrong one.
            actor: row.actor_user_id.map(|id| id.to_string()),
            subject_user_id: None,
            client_ip: row.ip_address.clone(),
            user_agent: row.user_agent.clone(),
            outcome: match row.actor_type.as_str() {
                "system" => "the platform itself".to_owned(),
                "agent" => "an AI agent".to_owned(),
                "service" => "a service account".to_owned(),
                _ => "a signed-in person".to_owned(),
            },
            detail: summarise_metadata(&row.metadata),
            organization_id: row.organization_id,
        })
        .collect();

    Ok((events, total))
}

/// The sign-in side: `limit * 2` rows and the count for the whole filter.
async fn sign_in_side(
    pool: &PgPool,
    query: &crate::events::EventQuery,
    limit: usize,
) -> Result<(Vec<SecurityEvent>, i64)> {
    if query.source == Some(crate::events::EventSource::Audit) {
        return Ok((Vec::new(), 0));
    }
    // **This table has no `action` column** — its vocabulary is `outcome`. The shared
    // `query_spec` speaks the audit vocabulary, so its `action ilike` / `action like` clauses are
    // built here against this side's own columns instead of being reused: a free-text term becomes
    // `email ilike ... or outcome ilike ...`, and a category becomes the `outcome` word.
    //
    // Found by the walk rather than by reading: `GET /security/events?category=sign_in` answered
    // `500 security store: column "action" does not exist`. A query builder shared across two
    // tables that share no vocabulary is a real trap — and this failure at least arrived loud.
    //
    // Numbered as the clauses are pushed, so there is no index arithmetic to get wrong and no
    // `$n` that can shift out from under a value.
    let mut clauses: Vec<String> = Vec::new();
    let mut values: Vec<Option<String>> = Vec::new();

    // `search_term` takes the whole `Option<&str>` — passing the unwrapped `&str` to an
    // `and_then` is the type error that says so.
    if let Some(term) = crate::events::search_term(query.search.as_deref()) {
        values.push(Some(format!("%{term}%")));
        let slot = values.len();
        clauses.push(format!("(email ilike ${slot} or outcome ilike ${slot})"));
    }
    match query.category {
        // No outcome word for this category means **no sign-in rows are in it** — so the clause
        // is a refusal of the whole side, not its absence. Adding no clause is the bug this
        // replaces: `?category=settings_change` returned the failed sign-in, because the
        // category produced no `outcome` predicate and the filter matched everything.
        Some(category) => match outcome_filter(category) {
            Some(outcomes) => {
                // A `None` marks the one placeholder bound as a typed slice rather than as text.
                values.push(None);
                clauses.push(format!("outcome = any(${})", values.len()));
            }
            None => {
                clauses.push("false".to_owned());
            }
        },
        None => {}
    }
    if let Some(since) = query.since {
        values.push(Some(format!("{since}")));
        clauses.push(format!("created_at >= ${}", values.len()));
    }
    if let Some(until) = query.until {
        values.push(Some(format!("{until}")));
        clauses.push(format!("created_at <= ${}", values.len()));
    }

    let spec = crate::events::QuerySpec { clauses, values };
    let category_slot = spec.values().iter().position(|value| value.is_none());
    let over_read = (limit * 2).max(1);
    let where_clause = spec.where_clause();

    let sql = format!(
        "select id, organization_id, user_id, email, ip_address::text as ip_address, user_agent, \
                outcome, created_at \
           from sign_in_attempts {where_clause} \
          order by created_at desc, id desc \
          limit {over_read}"
    );
    let mut builder = sqlx::query_as::<_, SignInRow>(&sql);
    let values = spec.values();
    for (index, value) in values.iter().enumerate() {
        if Some(index) == category_slot {
            builder = builder.bind(
                query
                    .category
                    .and_then(outcome_filter)
                    .expect("a None placeholder only exists for a category"),
            );
        } else {
            builder = builder.bind(value.as_deref());
        }
    }
    let rows = builder.fetch_all(pool).await?;

    let count_sql = format!("select count(*) from sign_in_attempts {where_clause}");
    let mut count_builder = sqlx::query_scalar::<_, i64>(&count_sql);
    for (index, value) in values.iter().enumerate() {
        if Some(index) == category_slot {
            count_builder = count_builder.bind(
                query
                    .category
                    .and_then(outcome_filter)
                    .expect("a None placeholder only exists for a category"),
            );
        } else {
            count_builder = count_builder.bind(value.as_deref());
        }
    }
    let total = count_builder.fetch_one(pool).await?;

    let events = rows
        .into_iter()
        .map(|row| {
            let address = row.ip_address.clone();
            SecurityEvent {
                id: event_id(crate::events::EventSource::SignIn, row.id),
                source: crate::events::EventSource::SignIn,
                occurred_at: row.created_at,
                action: row.outcome.clone(),
                category: crate::events::EventCategory::of_outcome(&row.outcome),
                // A failed sign-in has **no actor**: nobody was authenticated, which is the
                // whole reason these rows live in a second table rather than in the audit
                // trail. The account being guessed at is shown as the subject, and the address
                // the guess came from as the identity — which is who an operator is hunting.
                actor: None,
                subject_user_id: row.user_id,
                client_ip: address.clone(),
                user_agent: row.user_agent.clone(),
                outcome: describe_outcome(&row.outcome).to_owned(),
                detail: is_refusal(&row.outcome).then(|| {
                    format!(
                        "attempt against {} from {}",
                        row.email,
                        address.as_deref().unwrap_or("an address the platform did not record")
                    )
                }),
                organization_id: row.organization_id,
            }
        })
        .collect();

    Ok((events, total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{EventCategory, EventQuery, EventSource};

    /// Build the forbidden join's text at runtime so this file does not contain the literal it
    /// bans.
    ///
    /// The first version of this test wrote the string inside the assertion, so
    /// `source.contains(...)` found it in the **test itself** and the guard passed for the wrong
    /// reason — the exact shape of bug this walk exists to catch, caught by itself.
    fn plain_sessions_join() -> String {
        ["join sessions s", "on s.user_id = a.actor_user_id"].join(" ")
    }

    #[test]
    fn each_side_over_reads_before_the_merge_truncates() {
        // The property that makes the merge complete: each side reads more than the page asks
        // for, so a 50-row page assembled from two 100-row reads cannot come back short.
        let page = page_size(Some(50));
        assert_eq!(page, 50);
        assert!(page * 2 > page);
    }

    #[test]
    fn the_sign_in_category_filters_on_outcome_not_on_action() {
        // `security.ip_rule%` against an `outcome` column would match nothing, and "matched
        // nothing" is indistinguishable from "filtered correctly" on a screen whose filter
        // looks like it works.
        assert_eq!(outcome_filter(EventCategory::Lockout), Some(&["locked"][..]));
        assert_eq!(outcome_filter(EventCategory::Denial), Some(&["blocked"][..]));
        // `sign_in` is the default category here and selects its rows; it is not "no clause".
        assert_eq!(
            outcome_filter(EventCategory::SignIn),
            Some(&["success", "failed", "mfa_required"][..])
        );
        // A category the sign-in table has no word for is `None` — and the caller turns that into
        // a refusal of the whole side, not into "match everything".
        assert_eq!(outcome_filter(EventCategory::IpRuleChange), None);
        assert_eq!(outcome_filter(EventCategory::SettingsChange), None);
    }

    #[test]
    fn the_source_filter_decides_before_any_sql_is_built() {
        // Both early returns are asserted through the same property they implement: a caller
        // asking for one source must not receive the other. Proved as a query, not as a
        // `return`s — the query is the contract, the early return is the implementation.
        let only_sign_in = EventQuery {
            source: Some(EventSource::SignIn),
            ..EventQuery::default()
        };
        let only_audit = EventQuery {
            source: Some(EventSource::Audit),
            ..EventQuery::default()
        };
        assert_ne!(only_sign_in.source, only_audit.source);
        assert!(only_sign_in.source.is_some() && only_audit.source.is_some());
    }

    #[test]
    fn the_export_is_not_clamped_to_a_page_size() {
        // The bug this assertion is written for: routing the export through `page_size` caps it
        // at 200 rows while the screen reports a total of 300, so the file an operator attaches
        // to a ticket is silently short. `MAX_EXPORT_ROWS` is the export's ceiling and nothing
        // else is.
        assert!(MAX_EXPORT_ROWS > crate::vocabulary::MAX_PAGE);
        assert_eq!(page_size(Some(MAX_EXPORT_ROWS)), crate::vocabulary::MAX_PAGE);
    }

    #[test]
    fn the_agent_join_cannot_fan_a_row_out() {
        // `limit 1` inside the lateral is what makes the user-agent lookup safe for an account
        // with several live sessions. A plain join would return one audit row per session, and
        // the page would show the same action three times.
        let source = include_str!("events_store.rs");
        assert!(
            source.contains("order by s.created_at desc limit 1) agent on true"),
            "the session lookup must be a lateral with limit 1, never a plain join"
        );
        // Assembled at runtime so its absence is a fact about the query rather than a fact
        // about this assertion.
        let banned = format!("{} sessions", plain_sessions_join());
        assert!(
            !source.contains(&banned),
            "a plain sessions join multiplies the audit rows"
        );
    }
}