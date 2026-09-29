//! Assignment rules, SLA policies and the arithmetic between a lead and a deadline.
//!
//! REQ-117, slice 2. Two things decide what happens to a lead after it lands: *who* gets it
//! and *how long* they have. Both are configuration an operator edits, both are evaluated
//! server-side, and both have to be predictable enough that an operator can predict them —
//! which is why the simulator ([`AssignmentOutcome`]) and the deadline ([`due_at`]) are pure
//! functions of their inputs and unit-tested as such rather than only through the API.
//!
//! ## The three rules
//!
//! 1. **Order is the semantics.** Rules evaluate top-down and the first match wins, so
//!    reordering the table is a behaviour change and the reorder endpoint renumbers densely.
//! 2. **The round-robin cursor moves under a row lock.** Fairness is a property of the claim
//!    transaction, not of the caller; see the migration's note 1.
//! 3. **A deadline is a wall-clock instant, always stored in UTC.** The business-hours window
//!    decides *which* instants count, and the viewer renders them in their own zone. Storing a
//!    local naive datetime is the bug this design exists to prevent: "due Monday 09:00" means
//!    nothing without the zone it was computed in.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::{Date, Duration, OffsetDateTime, PrimitiveDateTime, Weekday};
use uuid::Uuid;

use crate::error::{CrmIntakeError, Result};
use crate::vocabulary::{MAX_BULK_IDS, is_round_robin_target};

/// An ordered assignment rule. `position` is the evaluation order; ties are broken by `id`
/// so evaluation is deterministic even if two rows were written with the same position.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow, Serialize, Deserialize)]
pub struct AssignmentRule {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub name: String,
    pub position: i32,
    pub conditions: Value,
    pub target_kind: String,
    pub target_user_id: Option<Uuid>,
    pub pool_user_ids: Vec<Uuid>,
    pub round_robin_cursor: i32,
    pub active: bool,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

impl AssignmentRule {
    /// The people this rule can hand a lead to, in the operator's order. `queue` has none —
    /// that *is* the difference between "chosen to be unassigned" and "nobody claimed it".
    pub fn candidate_user_ids(&self) -> Vec<Uuid> {
        match self.target_kind.as_str() {
            "user" => self.target_user_id.into_iter().collect(),
            "pool" => self.pool_user_ids.clone(),
            _ => Vec::new(),
        }
    }

    /// One line for the panel's "conditions" chips. Never empty for a catch-all: an operator
    /// reading "matches everything" learns nothing, "no conditions" tells them the rule is
    /// the floor and everything else sits above it.
    pub fn condition_summary(&self) -> String {
        let map = match self.conditions.as_object() {
            Some(m) if !m.is_empty() => m,
            _ => return "no conditions (matches every lead)".to_string(),
        };
        let mut parts: Vec<String> = Vec::new();
        for (key, value) in map {
            if key == "has_email" {
                if let Some(b) = value.as_bool() {
                    parts.push(if b {
                        "has an e-mail".into()
                    } else {
                        "has no e-mail".into()
                    });
                }
                continue;
            }
            match value.as_array() {
                Some(items) if !items.is_empty() => {
                    let words: Vec<String> = items
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect();
                    if !words.is_empty() {
                        parts.push(format!("{key} in {}", words.join(", ")));
                    }
                }
                // An empty array is "matches nothing on this key", which is a rule an
                // operator wrote by accident. Saying it out loud is the difference between a
                // dead rule and a discovered one.
                Some(_) => parts.push(format!("{key} in (none) — matches nothing")),
                None => {
                    if let Some(s) = value.as_str() {
                        parts.push(format!("{key} = {s}"));
                    }
                }
            }
        }
        if parts.is_empty() {
            "no conditions (matches every lead)".to_string()
        } else {
            parts.join(" · ")
        }
    }

    /// Whether a payload satisfies this rule's conditions. Pure, and the reason the simulator
    /// can promise to answer without writing.
    pub fn matches(&self, payload: &AssignmentInput) -> bool {
        conditions_match(&self.conditions, payload)
    }
}

/// The facts a rule may condition on, read from a lead. Everything is optional because a
/// visitor may legitimately have given a name and nothing else: an absent key matches
/// everything, a key present with an empty list matches nothing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AssignmentInput {
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub product_interest: Option<String>,
    #[serde(default)]
    pub budget_band: Option<String>,
    #[serde(default)]
    pub source_id: Option<Uuid>,
    #[serde(default)]
    pub source_name: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub has_email: Option<bool>,
}

impl AssignmentInput {
    /// Read a lead-shaped jsonb row into the fields a rule can condition on. Written as a
    /// lookup rather than a struct so a lead that gains a column does not break this.
    pub fn from_lead_row(row: &Value) -> Self {
        let s = |key: &str| {
            row.get(key)
                .and_then(Value::as_str)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        Self {
            country: s("country"),
            region: s("region"),
            product_interest: s("product_interest"),
            budget_band: s("budget_band"),
            source_id: row
                .get("source_id")
                .and_then(Value::as_str)
                .and_then(|v| Uuid::parse_str(v).ok()),
            source_name: s("source_name"),
            language: s("language"),
            has_email: Some(
                row.get("email")
                    .and_then(Value::as_str)
                    .is_some_and(|v| !v.trim().is_empty()),
            ),
        }
    }

    /// Build from a simulator's pasted payload. Accepts the lead's own column names and the
    /// friendly aliases the panel's form uses, because a pasted payload is written by a
    /// person and "the country" is what they will type.
    pub fn from_payload(payload: &Value) -> Self {
        let pick = |keys: &[&str]| -> Option<String> {
            keys.iter().find_map(|k| {
                payload
                    .get(*k)
                    .and_then(Value::as_str)
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
            })
        };
        Self {
            country: pick(&["country", "country_code", "Country"]),
            region: pick(&["region", "state", "Region"]),
            product_interest: pick(&["product_interest", "product", "productInterest"]),
            budget_band: pick(&["budget_band", "budget", "budgetBand"]),
            source_id: pick(&["source_id", "sourceId"]).and_then(|v| Uuid::parse_str(&v).ok()),
            source_name: pick(&["source_name", "source", "sourceName"]),
            language: pick(&["language", "lang", "Language"]),
            has_email: Some(pick(&["email", "e-mail", "Email"]).is_some()),
        }
    }
}

/// The evaluation of a conditions document against an input. Kept separate from
/// [`AssignmentRule::matches`] so the rules table's editor and the evaluator cannot drift.
pub fn conditions_match(conditions: &Value, input: &AssignmentInput) -> bool {
    let map = match conditions.as_object() {
        Some(m) => m,
        None => return true,
    };
    for (key, value) in map {
        let ok = match key.as_str() {
            "has_email" => match (value.as_bool(), input.has_email) {
                (Some(want), Some(have)) => want == have,
                // A rule conditioned on `has_email` against a payload that carries no
                // e-mail at all is a rule about a fact nobody told us. Treat it as a miss
                // rather than a hit: guessing "yes" would hand every anonymous submission
                // to a rule the operator meant for named people.
                _ => false,
            },
            "country" => list_contains(value, input.country.as_deref()),
            "region" => list_contains(value, input.region.as_deref()),
            "product_interest" => list_contains(value, input.product_interest.as_deref()),
            "budget_band" => list_contains(value, input.budget_band.as_deref()),
            "language" => list_contains(value, input.language.as_deref()),
            "source_name" => list_contains(value, input.source_name.as_deref()),
            "source_id" => match (value.as_array(), input.source_id) {
                (Some(items), Some(have)) => items.iter().any(|v| {
                    v.as_str()
                        .and_then(|s| Uuid::parse_str(s).ok())
                        .is_some_and(|want| want == have)
                }),
                // A rule keyed on a specific source cannot be satisfied by a payload that
                // names no source, whatever else matches.
                _ => false,
            },
            // An unknown key is a rule that cannot be evaluated. Failing closed (no match)
            // would silently drop the rule from the chain; failing open would let a typo
            // send every lead to a target the operator did not choose. Neither is acceptable,
            // so the validation function refuses to save it, and this arm is unreachable
            // in practice. It returns false and the validator is the real gate.
            _ => false,
        };
        if !ok {
            return false;
        }
    }
    true
}

/// A list condition: absent → match, empty list → no match, otherwise case-insensitive
/// membership. Case folding is not decoration: a rule written `DE` must catch a visitor who
/// typed `de`, or the rule reads as broken on the simulator and works in production.
fn list_contains(value: &Value, have: Option<&str>) -> bool {
    let items = match value.as_array() {
        Some(items) => items,
        // A scalar is a one-element list. Operators paste "TR" far more often than `["TR"]`.
        None => {
            if value.is_null() {
                return true;
            }
            return match (value.as_str(), have) {
                (Some(want), Some(have)) => want.eq_ignore_ascii_case(have),
                (Some(_), None) => false,
                _ => true,
            };
        }
    };
    if items.is_empty() {
        return false;
    }
    let have = match have {
        Some(h) => h,
        // The list is non-empty and the payload has nothing for it: no match, so a
        // country rule does not swallow leads that carry no country.
        None => return false,
    };
    items
        .iter()
        .filter_map(Value::as_str)
        .any(|want| want.eq_ignore_ascii_case(have))
}

/// What the evaluator decided, and why. The simulator renders this verbatim, so the
/// "why" is part of the type rather than a log line nobody sees.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssignmentOutcome {
    pub rule_id: Option<Uuid>,
    pub rule_name: Option<String>,
    pub target_kind: Option<String>,
    pub owner_user_id: Option<Uuid>,
    /// The evaluation order that produced this, 0-based. `None` means nothing matched.
    pub matched_index: Option<usize>,
    /// The rules that were evaluated and lost, with the key that made them miss. This is the
    /// part that makes the simulator worth having: "why didn't my other rule win" is the
    /// question an operator actually has.
    pub skipped: Vec<SkippedRule>,
    pub cursor_before: Option<i32>,
    pub cursor_after: Option<i32>,
}

impl AssignmentOutcome {
    /// The unassigned landing: no rule matched, so the lead waits in the visible queue.
    pub fn unassigned() -> Self {
        Self {
            rule_id: None,
            rule_name: None,
            target_kind: Some("queue".into()),
            owner_user_id: None,
            matched_index: None,
            skipped: Vec::new(),
            cursor_before: None,
            cursor_after: None,
        }
    }
}

/// A rule that was evaluated and did not match, with the first condition key that failed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SkippedRule {
    pub rule_id: Uuid,
    pub rule_name: String,
    pub failed_on: String,
}

/// Which condition key of a rule misses a given input — used by [`simulate`] to explain a
/// skip. Returns `None` when the rule matches.
pub fn first_failing_key(conditions: &Value, input: &AssignmentInput) -> Option<String> {
    let map = conditions.as_object()?;
    for (key, value) in map {
        let ok = match key.as_str() {
            "has_email" => match (value.as_bool(), input.has_email) {
                (Some(want), Some(have)) => want == have,
                _ => false,
            },
            "country" => list_contains(value, input.country.as_deref()),
            "region" => list_contains(value, input.region.as_deref()),
            "product_interest" => list_contains(value, input.product_interest.as_deref()),
            "budget_band" => list_contains(value, input.budget_band.as_deref()),
            "language" => list_contains(value, input.language.as_deref()),
            "source_name" => list_contains(value, input.source_name.as_deref()),
            "source_id" => match (value.as_array(), input.source_id) {
                (Some(items), Some(have)) => items.iter().any(|v| {
                    v.as_str()
                        .and_then(|s| Uuid::parse_str(s).ok())
                        .is_some_and(|want| want == have)
                }),
                _ => false,
            },
            _ => false,
        };
        if !ok {
            return Some(key.clone());
        }
    }
    None
}

/// Pick the winning rule for a payload without touching any state. `cursor_before` is the
/// cursor the claim *would* read, so the simulator can show which pool member a lead would go
/// to right now — the number it prints is the number the next lead actually gets.
pub fn simulate(rules: &[AssignmentRule], input: &AssignmentInput) -> AssignmentOutcome {
    let mut ordered: Vec<&AssignmentRule> = rules.iter().filter(|r| r.active).collect();
    ordered.sort_by(|a, b| a.position.cmp(&b.position).then(a.id.cmp(&b.id)));

    let mut skipped = Vec::new();
    for rule in &ordered {
        if rule.matches(input) {
            let (owner, cursor_before, cursor_after) = match rule.target_kind.as_str() {
                // A `user` rule whose person was deleted is treated the same way an emptied
                // pool is: a skip, so the next rule gets its turn. The alternative — a match
                // with no owner — is the one outcome an assignment chain must never produce,
                // and it is reachable in production (delete the user, keep the rule) as
                // surely as the pool case. The migration's check refuses to *save* such a
                // rule; this arm is for the one that was already saved.
                "user" if rule.target_user_id.is_none() => {
                    skipped.push(SkippedRule {
                        rule_id: rule.id,
                        rule_name: rule.name.clone(),
                        failed_on: "target (the person this rule names is gone)".into(),
                    });
                    continue;
                }
                "user" => (rule.target_user_id, None, None),
                "pool" => {
                    let pool = rule.candidate_user_ids();
                    if pool.is_empty() {
                        // The migration's check refuses to save an empty pool, so this arm
                        // means the pool was emptied by deleting the users it pointed at.
                        // Treating it as a miss lets the next rule try, which is what an
                        // operator whose users were deleted expects.
                        skipped.push(SkippedRule {
                            rule_id: rule.id,
                            rule_name: rule.name.clone(),
                            failed_on: "pool (no active members)".into(),
                        });
                        continue;
                    }
                    let cursor = rule.round_robin_cursor;
                    let idx = cursor.rem_euclid(pool.len() as i32) as usize;
                    (
                        pool.get(idx).copied(),
                        Some(cursor),
                        Some((cursor + 1) % pool.len() as i32),
                    )
                }
                _ => (None, None, None),
            };
            let matched_index = ordered.iter().position(|r| r.id == rule.id);
            return AssignmentOutcome {
                rule_id: Some(rule.id),
                rule_name: Some(rule.name.clone()),
                target_kind: Some(rule.target_kind.clone()),
                owner_user_id: owner,
                matched_index,
                skipped,
                cursor_before,
                cursor_after,
            };
        }
        let failed_on =
            first_failing_key(&rule.conditions, input).unwrap_or_else(|| "conditions".to_string());
        skipped.push(SkippedRule {
            rule_id: rule.id,
            rule_name: rule.name.clone(),
            failed_on,
        });
    }
    AssignmentOutcome::unassigned()
}

/// A first-response target.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow, Serialize, Deserialize)]
pub struct SlaPolicy {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub name: String,
    pub first_response_minutes: i32,
    pub business_hours_only: bool,
    pub reminder_minutes: Option<i32>,
    pub escalate_to_user_id: Option<Uuid>,
    pub business_hours: Value,
    pub active: bool,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// The weekly window a policy counts its minutes in. `None` days or an unparsable time means
/// "always open", which is the honest default for a window nobody configured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BusinessHours {
    /// ISO weekdays: 1 = Monday … 7 = Sunday.
    pub days: Vec<u8>,
    pub start: String,
    pub end: String,
    pub timezone: String,
}

impl BusinessHours {
    pub fn from_value(value: &Value) -> Option<Self> {
        let map = value.as_object()?;
        let days = map
            .get("days")?
            .as_array()?
            .iter()
            .filter_map(Value::as_u64)
            .map(|d| d as u8)
            .filter(|d| (1..=7).contains(d))
            .collect();
        Some(Self {
            days,
            start: map.get("start")?.as_str()?.to_string(),
            end: map.get("end")?.as_str()?.to_string(),
            timezone: map
                .get("timezone")
                .and_then(Value::as_str)
                .unwrap_or("UTC")
                .to_string(),
        })
    }

    pub fn to_value(&self) -> Value {
        serde_json::json!({
            "days": self.days,
            "start": self.start,
            "end": self.end,
            "timezone": self.timezone,
        })
    }

    fn opens_at(&self) -> Option<(i64, i64)> {
        Some((parse_hhmm(&self.start)?, parse_hhmm(&self.end)?))
    }
}

/// `time`'s crate has no civil-timezone maths, and pulling a full tz database in for "add
/// N business hours" is the wrong trade for a panel deadline. The policy's window is read in
/// UTC and the *label* keeps the operator's zone name, which is honest: the deadline is a
/// wall-clock instant, and the rule is stated in the organization's own words. A zone-aware
/// recomputation is a follow-up, not a silent approximation — the field is named
/// `timezone` and the panel says it is the organization's label for now.
fn iso_weekday(at: OffsetDateTime) -> Option<u8> {
    Some(match at.weekday() {
        Weekday::Monday => 1,
        Weekday::Tuesday => 2,
        Weekday::Wednesday => 3,
        Weekday::Thursday => 4,
        Weekday::Friday => 5,
        Weekday::Saturday => 6,
        Weekday::Sunday => 7,
    })
}

/// "09:00" → 540 minutes past midnight.
///
/// The return is `i64`, not `u8`: a `u8` of minutes wraps at 256, so a 09:00 start would
/// read as 28 and every window would compare against the wrong number. This is the same
/// class of bug as the duplicate migration version — a value that looks right in a narrow
/// test and is wrong in the general case.
fn parse_hhmm(value: &str) -> Option<i64> {
    let (h, m) = value.split_once(':')?;
    let h: i64 = h.trim().parse().ok()?;
    let m: i64 = m.trim().parse().ok()?;
    if m > 59 || h > 24 {
        return None;
    }
    Some(h * 60 + m)
}

/// The deadline a policy sets for a lead received at `received_at`.
///
/// Two behaviours are deliberate:
///
/// * a policy that is **not** business-hours-only is plain wall-clock addition, and
/// * a window with no opening time is treated as always open rather than as "never open",
///   so a half-configured policy does not park every lead in a queue nobody can see.
pub fn due_at(policy: &SlaPolicy, received_at: OffsetDateTime) -> OffsetDateTime {
    let minutes = Duration::minutes(i64::from(policy.first_response_minutes));
    if !policy.business_hours_only {
        return received_at + minutes;
    }
    let window = match policy
        .business_hours
        .as_object()
        .and_then(|_| BusinessHours::from_value(&policy.business_hours))
    {
        Some(w) => w,
        None => return received_at + minutes,
    };
    let (open, close) = match window.opens_at() {
        Some(v) => v,
        None => return received_at + minutes,
    };
    if close <= open {
        // A window that ends before it opens is not a window; treat the policy as plain
        // wall-clock rather than looping forever over an empty set of open minutes.
        return received_at + minutes;
    }
    add_business_minutes(received_at, minutes, &window, open, close)
}

/// The SLA state shown on the inbox and in the lead detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlaState {
    OnTrack,
    AtRisk,
    Breached,
    Met,
}

impl SlaState {
    /// The state of a lead, from three facts and nothing else: was it answered, when is the
    /// deadline, and is it still a lead somebody can answer.
    ///
    /// `at_risk` is 25% of the window remaining rather than a fixed number of minutes,
    /// because "15 minutes left" means something different for a 4-hour target and a
    /// 15-minute one, and a fixed threshold would mark every short-window policy breached
    /// before the operator ever saw it.
    pub fn of(
        first_response_at: Option<OffsetDateTime>,
        due_at: Option<OffsetDateTime>,
        now: OffsetDateTime,
        window_minutes: i32,
    ) -> Self {
        if first_response_at.is_some() {
            return Self::Met;
        }
        let due = match due_at {
            Some(d) => d,
            None => return Self::OnTrack,
        };
        if now >= due {
            return Self::Breached;
        }
        let remaining = (due - now).whole_minutes().max(0);
        // A quarter of the window, floored at a quarter of an hour. The floor is not
        // decoration: with the ratio alone a 15-minute policy stays "on track" until three
        // minutes are left, which is not a warning, while a 4-hour policy warns with an hour
        // left. Both ends of that scale need a warning early enough to act on.
        let threshold = (i64::from(window_minutes) / 4).max(15);
        if remaining <= threshold {
            Self::AtRisk
        } else {
            Self::OnTrack
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnTrack => "on_track",
            Self::AtRisk => "at_risk",
            Self::Breached => "breached",
            Self::Met => "met",
        }
    }
}

/// Validation for a rule an operator is saving, so the refusal arrives at the editor with
/// the offending field named rather than as a constraint error at the database.
pub fn validate_rule(
    name: &str,
    conditions: &Value,
    target_kind: &str,
    target_user_id: Option<Uuid>,
    pool_user_ids: &[Uuid],
) -> Result<()> {
    if name.trim().is_empty() {
        return Err(CrmIntakeError::Invalid("the rule needs a name".into()));
    }
    if !is_round_robin_target(target_kind) {
        return Err(CrmIntakeError::Invalid(format!(
            "'{target_kind}' is not a target; use user, pool or queue"
        )));
    }
    match target_kind {
        "user" => {
            if target_user_id.is_none() {
                return Err(CrmIntakeError::Invalid(
                    "a rule that targets a person needs one".into(),
                ));
            }
        }
        "pool" => {
            if pool_user_ids.is_empty() {
                return Err(CrmIntakeError::Invalid(
                    "a round-robin pool needs at least one person".into(),
                ));
            }
            let mut seen = pool_user_ids.to_vec();
            seen.sort();
            seen.dedup();
            if seen.len() != pool_user_ids.len() {
                return Err(CrmIntakeError::Invalid(
                    "the pool lists the same person twice, so they would get two slots".into(),
                ));
            }
        }
        _ => {}
    }
    if let Some(map) = conditions.as_object() {
        for key in map.keys() {
            if !matches!(
                key.as_str(),
                "country"
                    | "region"
                    | "product_interest"
                    | "budget_band"
                    | "source_id"
                    | "source_name"
                    | "language"
                    | "has_email"
            ) {
                return Err(CrmIntakeError::Invalid(format!(
                    "'{key}' is not a condition a lead carries; use country, region, \
product_interest, budget_band, source_id, source_name, language or has_email"
                )));
            }
        }
    } else if !conditions.is_null() {
        return Err(CrmIntakeError::Invalid(
            "conditions must be an object".into(),
        ));
    }
    Ok(())
}

/// Validation for a saved SLA policy. The reminder rule is the one people get wrong: a
/// reminder at the same minute as the breach is a reminder nobody reads, so it is refused
/// rather than silently accepted and fired together with the escalation.
pub fn validate_policy(
    name: &str,
    first_response_minutes: i32,
    reminder_minutes: Option<i32>,
    business_hours: &Value,
) -> Result<()> {
    if name.trim().is_empty() {
        return Err(CrmIntakeError::Invalid("the policy needs a name".into()));
    }
    if !(1..=20160).contains(&first_response_minutes) {
        return Err(CrmIntakeError::Invalid(
            "the first-response target must be between 1 minute and 14 days".into(),
        ));
    }
    if let Some(reminder) = reminder_minutes {
        if !(1..=20160).contains(&reminder) {
            return Err(CrmIntakeError::Invalid(
                "the reminder must be between 1 minute and 14 days".into(),
            ));
        }
        if reminder == first_response_minutes {
            return Err(CrmIntakeError::Invalid(
                "the reminder cannot be at the same minute as the breach — pick an earlier one"
                    .into(),
            ));
        }
    }
    if let Some(window) = BusinessHours::from_value(business_hours) {
        if let Some((open, close)) = window.opens_at() {
            if close <= open {
                return Err(CrmIntakeError::Invalid(
                    "the business-hours window ends before it starts".into(),
                ));
            }
        }
        if !window.days.is_empty() && !window.days.iter().all(|d| (1..=7).contains(d)) {
            return Err(CrmIntakeError::Invalid(
                "business days must be 1 (Monday) to 7 (Sunday)".into(),
            ));
        }
    } else if business_hours.as_object().is_some_and(|m| !m.is_empty()) {
        return Err(CrmIntakeError::Invalid(
            "business hours need days, start and end".into(),
        ));
    }
    Ok(())
}

/// Walk the clock forward one business minute at a time until `minutes` have been spent
/// inside an open window. Minute-by-minute is not clever, and it is exactly right: a
/// business-hours deadline is a small number (the cap is two weeks) and the naive version
/// cannot be wrong about a DST shift because it never converts a local time to UTC.
fn add_business_minutes(
    start: OffsetDateTime,
    minutes: Duration,
    window: &BusinessHours,
    open: i64,
    close: i64,
) -> OffsetDateTime {
    let mut at = start;
    let mut left = minutes.whole_minutes();
    // Bounded so a window that is never open cannot spin: 14 days of real time past the
    // start covers the largest legal target even with a one-hour-a-day window.
    let ceiling = 14 * 24 * 60 + 1;
    let mut guard = 0;
    while left > 0 && guard < ceiling {
        guard += 1;
        if !in_window(at, window, open, close) {
            at = next_open(at, window, open);
            continue;
        }
        at += Duration::minutes(1);
        left -= 1;
    }
    at
}

fn minute_of_day(at: OffsetDateTime) -> i64 {
    let t = at.time();
    i64::from(t.hour()) * 60 + i64::from(t.minute())
}

fn in_window(at: OffsetDateTime, window: &BusinessHours, open: i64, close: i64) -> bool {
    let day = match iso_weekday(at) {
        Some(d) => d,
        None => return false,
    };
    if !window.days.is_empty() && !window.days.contains(&day) {
        return false;
    }
    let m = minute_of_day(at);
    open <= m && m < close
}

/// The next instant at or after `at` that the window is open. Steps to midnight and walks
/// forward up to a week, which is bounded and cannot loop on a window with no open days
/// because the caller's `ceiling` ends the outer walk.
/// The next instant at or after `at` that the window is open, found by stepping to each
/// candidate day's opening time. At most eight steps: a window with no open day inside a
/// week returns the eighth day's opening, and the caller's outer walk ends long before that
/// can produce a wrong answer — it degrades to "a very late deadline" rather than a hang.
fn next_open(at: OffsetDateTime, window: &BusinessHours, open: i64) -> OffsetDateTime {
    let mut day_start = start_of_day(at);
    for _ in 0..8 {
        let day = iso_weekday(day_start);
        let open_today = window.days.is_empty() || day.is_some_and(|d| window.days.contains(&d));
        let opens = day_start + Duration::minutes(open);
        if open_today && opens >= at {
            return opens;
        }
        day_start += Duration::days(1);
    }
    at
}

fn start_of_day(at: OffsetDateTime) -> OffsetDateTime {
    let d: Date = at.date();
    PrimitiveDateTime::new(d, time::macros::time!(00:00)).assume_offset(at.offset())
}

/// The next free position for a new rule: one past the current maximum. Written as a pure
/// function so the endpoint and the tests agree on what "append at the bottom" means.
pub fn next_position(rules: &[AssignmentRule]) -> i32 {
    rules.iter().map(|r| r.position).max().unwrap_or(0) + 1
}

/// Renumber a reordered list densely, preserving the caller's order.
///
/// Three decisions, each of which has bitten an implementation before:
///
/// * **Densely**, from zero. A reorder that left gaps would make the next append (which takes
///   max + 1) land in the wrong place after a delete.
/// * **The filter runs before the cap, not after.** Capping the *caller's* list at
///   `MAX_BULK_IDS` and then discarding the ids that are not real rules means a request
///   that opens with stale ids silently reorders nothing and still answers 200. The cap
///   is on the rows handed back, because that is the rows the caller is about to write.
/// * **The positions come from the filtered list.** Indexing the caller's array and then
///   filtering assigns a rule to the position of an id that was thrown away, which is exactly
///   the "why did my rule jump three rows" bug.
pub fn renumber(ids: &[Uuid], existing: &[AssignmentRule]) -> Vec<(Uuid, i32)> {
    let known: Vec<Uuid> = ids
        .iter()
        .copied()
        .filter(|id| existing.iter().any(|r| r.id == *id))
        .take(MAX_BULK_IDS)
        .collect();
    let mut seen = known.clone();
    seen.sort();
    let before = seen.len();
    seen.dedup();
    if seen.len() != before {
        // A repeated id is a caller bug, not something to paper over: renumbering it twice
        // gives one of the two rows a position the other does not have, and the evaluator's
        // tie-break on id then silently decides which of them a lead goes to.
        return Vec::new();
    }
    known
        .iter()
        .enumerate()
        .map(|(idx, id)| (*id, idx as i32))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(id: Uuid, position: i32, conditions: Value, kind: &str) -> AssignmentRule {
        let now = OffsetDateTime::UNIX_EPOCH;
        AssignmentRule {
            id,
            organization_id: Uuid::nil(),
            name: format!("rule {position}"),
            position,
            conditions,
            target_kind: kind.into(),
            target_user_id: None,
            pool_user_ids: vec![],
            round_robin_cursor: 0,
            active: true,
            created_at: now,
            updated_at: now,
        }
    }

    fn at(text: &str) -> OffsetDateTime {
        // "2026-01-05T09:00:00Z" → 2026-01-05 is a Monday.
        OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
            .expect("timestamp")
    }

    /// The four cases a conditions document can be in, which are genuinely four and not two.
    /// The third is the one that surprises people and is therefore the one the editor has to
    /// make visible: a condition on a key the payload says nothing about does **not** match,
    /// so a country rule cannot swallow the leads that carry no country.
    #[test]
    fn a_key_the_payload_is_silent_about_is_a_miss_not_a_pass() {
        let input = AssignmentInput {
            country: Some("TR".into()),
            ..Default::default()
        };
        // 1. The key is not in the document → no opinion → match.
        assert!(
            conditions_match(&json!({}), &input),
            "no conditions matches"
        );
        // 2. The key is present and the payload has the value → match.
        assert!(conditions_match(&json!({"country": ["TR"]}), &input));
        // 3. The key is present and the payload is silent → miss.
        assert!(
            !conditions_match(&json!({"region": ["Marmara"]}), &input),
            "a region rule must not match a lead that never said where it is"
        );
        // 4. The key is present with an empty list → a dead rule, which the editor renders as
        //    dead rather than as a rule that quietly works.
        assert!(
            !conditions_match(&json!({"country": []}), &input),
            "an empty list matches nothing"
        );
    }

    #[test]
    fn list_membership_ignores_case_in_both_directions() {
        let input = AssignmentInput {
            country: Some("tr".into()),
            ..Default::default()
        };
        assert!(conditions_match(&json!({"country": ["TR", "DE"]}), &input));
        let upper = AssignmentInput {
            country: Some("DE".into()),
            ..Default::default()
        };
        assert!(conditions_match(&json!({"country": ["de"]}), &upper));
    }

    #[test]
    fn a_list_never_matches_a_payload_that_says_nothing_about_it() {
        let input = AssignmentInput::default();
        assert!(!conditions_match(&json!({"country": ["TR"]}), &input));
    }

    #[test]
    fn a_scalar_condition_is_read_as_a_one_element_list() {
        let input = AssignmentInput {
            country: Some("TR".into()),
            ..Default::default()
        };
        assert!(conditions_match(&json!({"country": "TR"}), &input));
    }

    #[test]
    fn the_first_matching_rule_wins_and_the_losers_name_the_key() {
        let country_rule = rule(Uuid::from_u128(1), 0, json!({"country": ["TR"]}), "queue");
        let catch_all = rule(Uuid::from_u128(2), 1, json!({}), "queue");
        let input = AssignmentInput {
            country: Some("TR".into()),
            ..Default::default()
        };
        let outcome = simulate(&[catch_all.clone(), country_rule.clone()], &input);
        assert_eq!(outcome.rule_id, Some(country_rule.id));
        assert_eq!(
            outcome.matched_index,
            Some(0),
            "positions decide, not array order"
        );
        assert!(outcome.skipped.is_empty(), "nothing was skipped");
    }

    #[test]
    fn a_skipped_rule_names_the_condition_that_missed() {
        let country_rule = rule(Uuid::from_u128(1), 0, json!({"country": ["TR"]}), "queue");
        let catch_all = rule(Uuid::from_u128(2), 1, json!({}), "queue");
        let input = AssignmentInput {
            country: Some("DE".into()),
            ..Default::default()
        };
        let outcome = simulate(&[country_rule, catch_all], &input);
        assert_eq!(outcome.rule_id, Some(Uuid::from_u128(2)));
        assert_eq!(outcome.skipped.len(), 1);
        assert_eq!(outcome.skipped[0].failed_on, "country");
    }

    #[test]
    fn an_unmatched_payload_lands_unassigned_rather_than_on_a_wrong_rule() {
        let country_rule = rule(Uuid::from_u128(1), 0, json!({"country": ["TR"]}), "queue");
        let outcome = simulate(&[country_rule], &AssignmentInput::default());
        assert_eq!(outcome.rule_id, None);
        assert_eq!(outcome.target_kind.as_deref(), Some("queue"));
    }

    #[test]
    fn ten_leads_across_three_people_never_repeat_a_person_twice_in_a_row() {
        let mut pool = rule(Uuid::from_u128(9), 0, json!({}), "pool");
        pool.pool_user_ids = vec![Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3)];
        let input = AssignmentInput::default();
        let mut seen: Vec<Uuid> = Vec::new();
        // The simulator is pure: advancing the cursor here is exactly what the claim
        // transaction does to the row, so the distribution this asserts is the real one.
        for _ in 0..10 {
            let outcome = simulate(std::slice::from_ref(&pool), &input);
            seen.push(outcome.owner_user_id.expect("an owner"));
            pool.round_robin_cursor = outcome.cursor_after.expect("a cursor");
        }
        for pair in seen.windows(2) {
            assert_ne!(pair[0], pair[1], "a pool member took two leads in a row");
        }
        assert_eq!(seen[0], seen[3], "the pool is a cycle, not a shuffle");
        assert_eq!(seen[0], seen[6]);
        assert_eq!(seen[1], seen[4]);
    }

    #[test]
    fn a_user_rule_whose_person_was_deleted_falls_through_rather_than_matching_nobody() {
        // A rule that matches every lead and hands it to nobody is the one outcome the
        // chain must never produce. The migration's check refuses to *save* it; this is the
        // path for the rule that was already saved and then lost its person.
        let mut orphaned = rule(Uuid::from_u128(1), 0, json!({}), "user");
        orphaned.target_user_id = None;
        let catch_all = rule(Uuid::from_u128(2), 1, json!({}), "queue");
        let outcome = simulate(&[orphaned, catch_all], &AssignmentInput::default());
        assert_eq!(
            outcome.rule_id,
            Some(Uuid::from_u128(2)),
            "the orphaned rule must be skipped, not honoured as an empty owner"
        );
        assert_eq!(outcome.owner_user_id, None, "the queue really is nobody");
        assert!(
            outcome.skipped[0].failed_on.contains("the person"),
            "the skip names the reason: {}",
            outcome.skipped[0].failed_on
        );
    }

    #[test]
    fn a_pool_whose_members_were_all_deleted_falls_through_to_the_next_rule() {
        let mut empty = rule(Uuid::from_u128(1), 0, json!({}), "pool");
        empty.pool_user_ids = vec![];
        let catch_all = rule(Uuid::from_u128(2), 1, json!({}), "queue");
        let outcome = simulate(&[empty, catch_all], &AssignmentInput::default());
        assert_eq!(outcome.rule_id, Some(Uuid::from_u128(2)));
        assert!(outcome.skipped[0].failed_on.contains("pool"));
    }

    #[test]
    fn an_inactive_rule_is_not_evaluated_at_all() {
        let mut off = rule(Uuid::from_u128(1), 0, json!({}), "queue");
        off.active = false;
        let catch_all = rule(Uuid::from_u128(2), 1, json!({}), "queue");
        let outcome = simulate(&[off, catch_all], &AssignmentInput::default());
        assert_eq!(outcome.rule_id, Some(Uuid::from_u128(2)));
        assert!(outcome.skipped.is_empty(), "an inactive rule is not a skip");
    }

    #[test]
    fn has_email_never_guesses_for_a_payload_that_does_not_say() {
        let wants = rule(Uuid::from_u128(1), 0, json!({"has_email": true}), "queue");
        let outcome = simulate(std::slice::from_ref(&wants), &AssignmentInput::default());
        assert_eq!(outcome.rule_id, None, "an unknown is not a yes");
        let input = AssignmentInput {
            has_email: Some(false),
            ..Default::default()
        };
        assert_eq!(simulate(std::slice::from_ref(&wants), &input).rule_id, None);
    }

    fn policy(minutes: i32, business: bool, window: Value) -> SlaPolicy {
        let now = OffsetDateTime::UNIX_EPOCH;
        SlaPolicy {
            id: Uuid::nil(),
            organization_id: Uuid::nil(),
            name: "p".into(),
            first_response_minutes: minutes,
            business_hours_only: business,
            reminder_minutes: None,
            escalate_to_user_id: None,
            business_hours: window,
            active: true,
            created_at: now,
            updated_at: now,
        }
    }

    const WEEKDAYS_9_17: &str =
        r#"{"days":[1,2,3,4,5],"start":"09:00","end":"17:00","timezone":"Europe/Istanbul"}"#;

    #[test]
    fn a_plain_policy_is_plain_wall_clock() {
        let p = policy(240, false, json!({}));
        let got = due_at(&p, at("2026-01-05T09:00:00Z"));
        assert_eq!(got, at("2026-01-05T13:00:00Z"));
    }

    #[test]
    fn a_lead_inside_the_window_is_due_inside_the_window() {
        // Monday 10:00 + 4 business hours = Monday 14:00.
        let p = policy(240, true, serde_json::from_str(WEEKDAYS_9_17).unwrap());
        assert_eq!(
            due_at(&p, at("2026-01-05T10:00:00Z")),
            at("2026-01-05T14:00:00Z")
        );
    }

    #[test]
    fn a_lead_after_cutoff_is_due_the_next_working_morning() {
        // Monday 18:00 is past the 17:00 close: the clock starts Tuesday at 09:00, so
        // 4 hours is due at Tuesday 13:00.
        let p = policy(240, true, serde_json::from_str(WEEKDAYS_9_17).unwrap());
        assert_eq!(
            due_at(&p, at("2026-01-05T18:00:00Z")),
            at("2026-01-06T13:00:00Z")
        );
    }

    #[test]
    fn a_friday_evening_lead_is_due_monday_not_saturday() {
        // 2026-01-09 is a Friday. 18:00 Friday → 4 business hours → Monday 13:00.
        let p = policy(240, true, serde_json::from_str(WEEKDAYS_9_17).unwrap());
        assert_eq!(
            due_at(&p, at("2026-01-09T18:00:00Z")),
            at("2026-01-12T13:00:00Z")
        );
    }

    #[test]
    fn a_lead_that_lands_on_a_closed_day_waits_for_the_window_to_open() {
        // Sunday 2026-01-11, 10:00 + 1 hour = Monday 10:00.
        let p = policy(60, true, serde_json::from_str(WEEKDAYS_9_17).unwrap());
        assert_eq!(
            due_at(&p, at("2026-01-11T10:00:00Z")),
            at("2026-01-12T10:00:00Z")
        );
    }

    #[test]
    fn a_long_target_walks_over_a_weekend() {
        // 20 business hours from Monday 10:00 runs to Wednesday 14:00.
        let p = policy(1200, true, serde_json::from_str(WEEKDAYS_9_17).unwrap());
        assert_eq!(
            due_at(&p, at("2026-01-05T10:00:00Z")),
            at("2026-01-07T14:00:00Z")
        );
    }

    #[test]
    fn a_half_configured_window_is_plain_wall_clock_rather_than_never_open() {
        let p = policy(60, true, json!({"days": [1, 2, 3]}));
        assert_eq!(
            due_at(&p, at("2026-01-05T10:00:00Z")),
            at("2026-01-05T11:00:00Z")
        );
    }

    #[test]
    fn a_window_that_ends_before_it_starts_is_plain_wall_clock() {
        let p = policy(
            60,
            true,
            json!({"days":[1,2,3],"start":"18:00","end":"09:00"}),
        );
        assert_eq!(
            due_at(&p, at("2026-01-05T10:00:00Z")),
            at("2026-01-05T11:00:00Z")
        );
    }

    #[test]
    fn the_state_is_met_once_answered_and_breached_once_the_deadline_passes() {
        let due = at("2026-01-05T10:00:00Z");
        assert_eq!(
            SlaState::of(
                Some(at("2026-01-05T09:00:00Z")),
                Some(due),
                at("2026-01-06T00:00:00Z"),
                240
            ),
            SlaState::Met
        );
        assert_eq!(
            SlaState::of(None, Some(due), at("2026-01-05T10:00:01Z"), 240),
            SlaState::Breached
        );
        assert_eq!(
            SlaState::of(None, Some(due), at("2026-01-05T10:00:00Z"), 240),
            SlaState::Breached,
            "the deadline instant itself is breached, not merely at risk"
        );
    }

    #[test]
    fn at_risk_is_a_quarter_of_the_window_not_a_fixed_number_of_minutes() {
        let due = at("2026-01-05T12:00:00Z");
        // 4-hour window: 1 hour left (25%) is at risk.
        assert_eq!(
            SlaState::of(None, Some(due), at("2026-01-05T11:00:00Z"), 240),
            SlaState::AtRisk
        );
        // 2 hours left (50%) is still on track.
        assert_eq!(
            SlaState::of(None, Some(due), at("2026-01-05T10:00:00Z"), 240),
            SlaState::OnTrack
        );
        // A 15-minute window with 5 minutes left is at risk, where a fixed 15-minute
        // threshold would have called it on track and then breached it unannounced.
        let short_due = at("2026-01-05T08:15:00Z");
        assert_eq!(
            SlaState::of(None, Some(short_due), at("2026-01-05T08:10:00Z"), 15),
            SlaState::AtRisk
        );
    }

    #[test]
    fn a_lead_with_no_deadline_is_on_track_rather_than_breached() {
        assert_eq!(
            SlaState::of(None, None, at("2026-01-05T10:00:00Z"), 240),
            SlaState::OnTrack
        );
    }

    #[test]
    fn a_reminder_at_the_breach_minute_is_refused_with_the_reason() {
        let err = validate_policy("p", 240, Some(240), &json!({})).unwrap_err();
        assert!(err.to_string().contains("same minute"));
    }

    #[test]
    fn a_policy_with_no_name_or_impossible_window_is_refused() {
        assert!(validate_policy("  ", 240, None, &json!({})).is_err());
        assert!(
            validate_policy("p", 0, None, &json!({})).is_err(),
            "zero minutes is not a target"
        );
        assert!(
            validate_policy(
                "p",
                240,
                None,
                &json!({"days":[1],"start":"18:00","end":"09:00"})
            )
            .is_err()
        );
    }

    #[test]
    fn a_rule_with_an_unknown_condition_is_refused_by_name() {
        let err = validate_rule("r", &json!({"timezone": "TR"}), "queue", None, &[]).unwrap_err();
        assert!(err.to_string().contains("timezone"));
    }

    #[test]
    fn a_rule_that_targets_nothing_is_refused() {
        assert!(validate_rule("r", &json!({}), "user", None, &[]).is_err());
        assert!(validate_rule("r", &json!({}), "pool", None, &[]).is_err());
        assert!(validate_rule("r", &json!({}), "queue", None, &[]).is_ok());
    }

    #[test]
    fn a_pool_listing_one_person_twice_is_refused() {
        let twice = vec![Uuid::from_u128(1), Uuid::from_u128(1)];
        let err = validate_rule("r", &json!({}), "pool", None, &twice).unwrap_err();
        assert!(err.to_string().contains("twice"));
    }

    #[test]
    fn the_condition_summary_says_when_a_list_matches_nothing() {
        let r = rule(Uuid::nil(), 0, json!({"country": []}), "queue");
        assert!(r.condition_summary().contains("matches nothing"));
        let bare = rule(Uuid::nil(), 0, json!({}), "queue");
        assert!(bare.condition_summary().contains("no conditions"));
    }

    #[test]
    fn a_nine_oclock_window_is_nine_oclock_and_not_a_wrapped_byte() {
        // The regression this guards: parse_hhmm once returned minutes in a `u8`, so 09:00
        // (540) wrapped to 28 and every window compared against a number an operator never
        // typed. A 09:00–17:00 window is the most common one there is, so the wrap would
        // have shipped as "business hours are 00:28 to 00:17" — which reads as a closed
        // window and parks every lead forever.
        let p = policy(60, true, serde_json::from_str(WEEKDAYS_9_17).unwrap());
        assert_eq!(
            due_at(&p, at("2026-01-05T10:00:00Z")),
            at("2026-01-05T11:00:00Z"),
            "an hour inside a 09:00-17:00 window is an hour"
        );
    }

    #[test]
    fn a_midnight_to_midnight_window_covers_the_whole_day() {
        let p = policy(
            60,
            true,
            json!({"days":[1,2,3,4,5],"start":"00:00","end":"24:00"}),
        );
        assert_eq!(
            due_at(&p, at("2026-01-05T23:00:00Z")),
            at("2026-01-06T00:00:00Z")
        );
    }

    fn existing(ids: &[u128]) -> Vec<AssignmentRule> {
        ids.iter()
            .map(|n| {
                let mut r = rule(Uuid::from_u128(*n), 0, json!({}), "queue");
                r.id = Uuid::from_u128(*n);
                r
            })
            .collect()
    }

    #[test]
    fn renumber_densifies_the_caller_order_from_zero() {
        let live = existing(&[1, 2, 3]);
        let got = renumber(&[Uuid::from_u128(3), Uuid::from_u128(1)], &live);
        assert_eq!(
            got,
            vec![(Uuid::from_u128(3), 0), (Uuid::from_u128(1), 1)],
            "positions follow the caller's order and start at zero"
        );
    }

    #[test]
    fn renumber_drops_a_stale_id_before_the_cap_not_after_it() {
        // 200 stale ids then three real ones. Capping first and filtering second would keep
        // nothing and answer "reordered" while having reordered nothing at all.
        let live = existing(&[1, 2, 3]);
        let mut ids: Vec<Uuid> = (100..300).map(Uuid::from_u128).collect();
        ids.extend([Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3)]);
        let got = renumber(&ids, &live);
        assert_eq!(
            got,
            vec![
                (Uuid::from_u128(1), 0),
                (Uuid::from_u128(2), 1),
                (Uuid::from_u128(3), 2)
            ]
        );
    }

    #[test]
    fn renumber_refuses_a_repeated_id_rather_than_picking_a_winner() {
        let live = existing(&[1, 2]);
        let got = renumber(
            &[Uuid::from_u128(1), Uuid::from_u128(1), Uuid::from_u128(2)],
            &live,
        );
        assert!(got.is_empty(), "a duplicate id is refused, not resolved");
    }
}
