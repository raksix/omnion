//! Access-audit depth, anomaly detection and the SIEM feed
//! (docs/requests/REQ-125, slice 4).
//!
//! The request extends the secrets access log. **There is deliberately no second ledger here.**
//! The platform already has `audit_log` (migration 0001): append-only, with the actor, the
//! organization, the peer address and a metadata object, and every secrets operation already
//! writes to it. Migration 0032 adds four nullable columns to it — `request_id`, `lease_id`,
//! `deployment_key_id`, `pipeline` — and this module reads and reasons over that one table. A
//! secrets-specific table would have had to be reconciled with REQ-037's log when that request
//! lands, and the request itself names the failure mode it wants avoided: *"two redaction
//! implementations would drift"*. The same argument applies to the ledger.
//!
//! Three responsibilities, and the order they matter in is not the order they are named in:
//!
//! 1. **Joinability.** A refusal the caller was handed carries a `request_id` in its details. If
//!    the trail cannot be joined on it, the refusal is decoration. So the id is generated once
//!    and written to both the error and the row, and [`AuditFilter`] reads by it.
//! 2. **Anomalies are advisory, never a gate.** Four patterns — an off-hours reveal, a reveal
//!    burst, a first access from a new network, a principal that never held the secret. Each is a
//!    row with an acknowledge action. *Nothing in this module can refuse a reveal*, and the
//!    [`DetectorSettings::hard_rule_enforced`] flag exists for the request's optional hard rule
//!    while being read nowhere: a rule that blocks a reveal can lock an incident responder out
//!    at the worst moment, and that costs more than an unread advisory row.
//! 3. **The SIEM feed carries metadata only.** [`siem_record`] is a projection with an explicit
//!    allowlist, not a redaction pass over a whole row. An allowlist cannot leak by omission
//!    because a column that is not on it is not on the output; a redaction pass can, because the
//!    day someone adds a column to `audit_log` the pass has never heard of it. The integration
//!    test greps the serialized feed for a fixture value, so this is asserted rather than
//!    asserted-by-review.

use serde_json::{Value, json};
use sqlx::PgPool;
use time::{Date, Duration, OffsetDateTime};
use uuid::Uuid;

use crate::error::Result;

/// The action *namespaces* this surface owns.
///
/// This started as a list of six exact action names and that was wrong in a way no unit test
/// could see: the handlers went on to write `secret.lease.issued`, `secret.lease.revoked`,
/// `secret.credential.typed`, `secret.credential.validated`, `secret.root_key.rewrap_paused`,
/// `secret.root_key.rewrap_resumed` and `deployment_key.*` — none of which matched the list. The
/// screen silently dropped them. A list of names is a list that rots, and it rots invisibly: the
/// rows still existed, an operator just could not see them, and no count anywhere disagreed.
///
/// A namespace prefix cannot rot that way. A new handler that writes `secret.whatever` is visible
/// the moment it ships, and the *filter list the screen offers* is derived from the rows actually
/// present (see [`distinct_actions`]) rather than from a list written by hand — which is what the
/// "a control that offers something that does not exist is a dead control" rule asks for.
///
/// The names are still specific: `secret.` and `deployment_key.` belong to this crate, so a
/// crafted `?action=` cannot widen the read to another feature's audit rows.
pub const TRACKED_NAMESPACES: [&str; 2] = ["secret.", "deployment_key."];

/// The exact actions this module knows about, kept for the tests that assert the two agree on the
/// headline names. A row written by an older build stays readable rather than failing to
/// deserialize, which is why these are `&'static str` and not an enum.
pub mod actions {
    /// A value was read out of an envelope.
    pub const REVEALED: &str = "secret.revealed";
    /// An operation was refused.
    pub const DENIED: &str = "secret.access.denied";
    /// The root key ring was rotated.
    pub const ROOT_ROTATED: &str = "secret.root_key.rotated";
    /// A credential slot was assigned.
    pub const SLOT_CHANGED: &str = "secret.slot_changed";
    /// A deployment key was used by a pipeline.
    pub const DEPLOY_KEY_USE: &str = "secret.deploy_key_use";
    /// A lease was issued, redeemed or revoked.
    pub const LEASE_ISSUED: &str = "secret.lease.issued";
    /// A lease was redeemed by a machine identity.
    pub const LEASE_REDEEMED: &str = "secret.lease.redeemed";
    /// A lease was revoked.
    pub const LEASE_REVOKED: &str = "secret.lease.revoked";
}

/// The four patterns the detectors raise.
pub const PATTERNS: [&str; 4] = [
    "off_hours_reveal",
    "reveal_burst",
    "new_network",
    "unfamiliar_principal",
];

/// One anomaly flag, as the screen and the acknowledge action read it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AnomalyRow {
    /// Identity column.
    pub id: i64,
    /// The secret the pattern was observed on.
    pub secret_id: Option<Uuid>,
    /// Which pattern.
    pub pattern: String,
    /// `advisory` today; `blocking` is the slot the optional hard rule would use.
    pub severity: String,
    /// The actor the pattern was observed on.
    pub actor_user_id: Option<Uuid>,
    /// The peer address, rendered as text (`inet` has no direct text codec in this stack).
    pub address: Option<String>,
    /// What the detector saw. Metadata only.
    pub detail: Value,
    /// The request id of the operation that triggered it.
    pub request_id: Option<Uuid>,
    /// When the flag was raised.
    pub created_at: OffsetDateTime,
    /// Who acknowledged it.
    pub acknowledged_by: Option<Uuid>,
    /// When they did.
    pub acknowledged_at: Option<OffsetDateTime>,
    /// The secret's name, joined in for the screen.
    pub secret_name: Option<String>,
}

/// The detector thresholds, as read from `secret_audit_settings`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::FromRow)]
pub struct DetectorSettings {
    /// First hour of business hours, local time.
    pub business_hours_start: i32,
    /// First hour *after* business hours, local time. Equal to the start means "always".
    pub business_hours_end: i32,
    /// How many reveals of one secret inside an hour count as a burst.
    pub reveal_burst_per_hour: i32,
    /// The request's optional hard rule. Shipped off, and read by nobody here.
    pub hard_rule_enforced: bool,
    /// Whether a reveal from an unfamiliar address is flagged.
    pub detect_new_network: bool,
}

impl Default for DetectorSettings {
    /// The seeded row's values, duplicated here so a database that has not applied 0032 yet
    /// still answers with the documented defaults rather than a zero.
    fn default() -> Self {
        Self {
            business_hours_start: 8,
            business_hours_end: 19,
            reveal_burst_per_hour: 10,
            hard_rule_enforced: false,
            detect_new_network: true,
        }
    }
}

impl DetectorSettings {
    /// Whether `hour` (0–23) counts as business hours.
    ///
    /// A wrapping range is the normal case for an overnight operation: `22 → 6` means the shift
    /// that starts at ten at night and ends at six in the morning. The naive `start <= hour <
    /// end` test would call that range *always outside hours* and flag every single reveal of an
    /// overnight workload, which is the fastest way to make an operator disable the detector.
    #[must_use]
    pub fn within_business_hours(&self, hour: u8) -> bool {
        let hour = i32::from(hour);
        let (start, end) = (self.business_hours_start, self.business_hours_end);
        if start == end {
            return true;
        }
        if start < end {
            (start..end).contains(&hour)
        } else {
            // Wrapping: 22 → 6 is 22, 23, 0, 1, 2, 3, 4, 5.
            hour >= start || hour < end
        }
    }
}

/// What a single reveal was observed doing, as the detectors are told about it.
#[derive(Debug, Clone)]
pub struct RevealObservation {
    /// The secret that was read.
    pub secret_id: Uuid,
    /// Who read it.
    pub actor_user_id: Option<Uuid>,
    /// Where from, as text.
    pub address: Option<String>,
    /// The request id handed back to the caller.
    pub request_id: Uuid,
    /// When the reveal happened.
    pub at: OffsetDateTime,
    /// The local hour, 0–23. Passed in rather than derived: the store has an instant and the
    /// *operator* has a timezone, and a detector that silently uses UTC flags an operator in
    /// İğdır at 22:00 local as working at night. The API resolves the hour from the
    /// installation's configured offset.
    pub local_hour: u8,
}

/// The anomalies one observation raises. Empty is the normal case and is not an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaisedAnomalies {
    /// The flags to write, in the order they should be written.
    pub patterns: Vec<RaisedAnomaly>,
}

/// One flag to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaisedAnomaly {
    /// The pattern, one of [`PATTERNS`].
    pub pattern: &'static str,
    /// What the detector saw. Metadata only, never a value.
    pub detail: Value,
}

/// The four detectors, run as *pure functions* over counts the store already has.
///
/// They are separated from the queries on purpose: the query that answers "how many reveals has
/// this actor made of this secret" and the rule that turns that answer into a flag are different
/// questions, and only one of them can be tested without a database.
pub struct Detectors;

impl Detectors {
    /// The off-hours reveal.
    #[must_use]
    pub fn off_hours_reveal(
        settings: &DetectorSettings,
        observation: &RevealObservation,
    ) -> Option<RaisedAnomaly> {
        if settings.within_business_hours(observation.local_hour) {
            return None;
        }
        Some(RaisedAnomaly {
            pattern: "off_hours_reveal",
            detail: json!({
                "local_hour": observation.local_hour,
                "business_hours": [settings.business_hours_start, settings.business_hours_end],
            }),
        })
    }

    /// The reveal burst. `reveals_last_hour` includes the observation being judged, so a
    /// threshold of 1 flags the second reveal rather than the first.
    ///
    /// The observation is taken and unused: the signature is uniform across the four detectors
    /// so `evaluate` can call them as one list, and a rule that needs no per-reveal field
    /// should not be given one just to be called.
    #[must_use]
    pub fn reveal_burst(
        settings: &DetectorSettings,
        _observation: &RevealObservation,
        reveals_last_hour: i64,
    ) -> Option<RaisedAnomaly> {
        if reveals_last_hour < i64::from(settings.reveal_burst_per_hour) {
            return None;
        }
        Some(RaisedAnomaly {
            pattern: "reveal_burst",
            detail: json!({
                "reveals_last_hour": reveals_last_hour,
                "threshold": settings.reveal_burst_per_hour,
            }),
        })
    }

    /// The first access from a network this actor has not used for this secret before.
    ///
    /// `address` is compared exactly, not by subnet: an operator who has revealed from
    /// `10.0.0.0/8` and then from `192.0.2.7` has genuinely used a new network, and a
    /// prefix-based comparison would need a CIDR parser in a hot path for a rule that is advisory
    /// anyway. The *screen* renders the address; the flag's job is to say "not this one before".
    #[must_use]
    pub fn new_network(
        settings: &DetectorSettings,
        observation: &RevealObservation,
        known_addresses: i64,
    ) -> Option<RaisedAnomaly> {
        if !settings.detect_new_network || observation.address.is_none() || known_addresses > 0 {
            return None;
        }
        Some(RaisedAnomaly {
            pattern: "new_network",
            detail: json!({ "address": observation.address }),
        })
    }

    /// A principal that never held the secret before.
    ///
    /// A reveal with no actor at all (a system job, a runner) is *not* unfamiliar: a machine
    /// that holds a slot and resolves it every five minutes is the design working, and flagging
    /// it would produce one row per resolution forever.
    #[must_use]
    pub fn unfamiliar_principal(
        observation: &RevealObservation,
        prior_reveals_by_actor: i64,
    ) -> Option<RaisedAnomaly> {
        if observation.actor_user_id.is_none() || prior_reveals_by_actor > 0 {
            return None;
        }
        Some(RaisedAnomaly {
            pattern: "unfamiliar_principal",
            detail: json!({ "actor_user_id": observation.actor_user_id }),
        })
    }

    /// Every detector, for one observation, in a fixed order.
    ///
    /// The counts come from the store; a caller that has none of them passes zeros and gets the
    /// off-hours rule only, which is the honest degradation: a rule that needs history cannot run
    /// without it, and inventing a count would be worse than skipping the rule.
    #[must_use]
    pub fn evaluate(
        settings: &DetectorSettings,
        observation: &RevealObservation,
        counts: RevealCounts,
    ) -> RaisedAnomalies {
        let mut patterns = Vec::new();
        for anomaly in [
            Self::off_hours_reveal(settings, observation),
            Self::reveal_burst(settings, observation, counts.reveals_last_hour),
            Self::new_network(settings, observation, counts.known_addresses),
            Self::unfamiliar_principal(observation, counts.prior_reveals_by_actor),
        ]
        .into_iter()
        .flatten()
        {
            patterns.push(anomaly);
        }
        RaisedAnomalies { patterns }
    }
}

/// The history counts the detectors need, all three of them cheap point reads on one index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RevealCounts {
    /// Reveals of this secret by this actor in the last hour, including this one.
    pub reveals_last_hour: i64,
    /// Distinct addresses this actor has revealed this secret from.
    pub known_addresses: i64,
    /// Times this actor has revealed this secret, ever.
    pub prior_reveals_by_actor: i64,
}

/// The headline actions this crate writes, kept for tests and for the crate's own vocabulary.
///
/// This is **not** what the screen filters on — that is [`TRACKED_NAMESPACES`], so a handler that
/// adds a name is visible without editing a list. It exists so the constants have one home and so
/// a test can assert they all belong to the surface.
#[must_use]
pub fn tracked_actions() -> [&'static str; 8] {
    [
        actions::REVEALED,
        actions::DENIED,
        actions::ROOT_ROTATED,
        actions::SLOT_CHANGED,
        actions::DEPLOY_KEY_USE,
        actions::LEASE_ISSUED,
        actions::LEASE_REDEEMED,
        actions::LEASE_REVOKED,
    ]
}

/// One row of the audit screen, as the secrets surface reads it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AuditRow {
    /// Identity column of `audit_log`.
    pub id: i64,
    /// The action name.
    pub action: String,
    /// `secret`, `lease`, `deployment_key`, …
    pub target_type: Option<String>,
    /// The target's id as text.
    pub target_id: Option<String>,
    /// Who acted.
    pub actor_user_id: Option<Uuid>,
    /// `user`, `agent`, `service` or `system`.
    pub actor_type: String,
    /// The peer address, as text.
    pub ip_address: Option<String>,
    /// Structured detail. Metadata only — the redaction helper ran before storage.
    pub metadata: Value,
    /// The request id the caller was handed.
    pub request_id: Option<Uuid>,
    /// The lease this touched.
    pub lease_id: Option<Uuid>,
    /// The machine identity that spent it.
    pub deployment_key_id: Option<Uuid>,
    /// The pipeline identity as presented.
    pub pipeline: Option<String>,
    /// When it was recorded.
    pub created_at: OffsetDateTime,
}

/// A filter over the audit screen.
#[derive(Debug, Clone, Default)]
pub struct AuditFilter {
    /// Only rows whose action is one of these. Empty means "every tracked action".
    pub actions: Vec<String>,
    /// Only rows about this secret.
    pub secret_id: Option<Uuid>,
    /// Only rows by this actor.
    pub actor_user_id: Option<Uuid>,
    /// Only rows from this address.
    pub address: Option<String>,
    /// Only rows with this request id.
    pub request_id: Option<Uuid>,
    /// Only rows at or after this instant.
    pub since: Option<OffsetDateTime>,
    /// Only the most recent N.
    pub limit: Option<i64>,
}

/// Whether an action name belongs to this surface.
///
/// A prefix test, so a handler that starts writing `secret.something_new` is visible the moment
/// it ships. The alternative -- an enumerated list -- silently drops every name added after the
/// list was written, which is precisely how lease rows went missing from this screen.
#[must_use]
pub fn is_tracked(action: &str) -> bool {
    TRACKED_NAMESPACES
        .iter()
        .any(|namespace| action.starts_with(namespace))
}

/// Read the audit trail, filtered, newest first.
///
/// Only actions in [`TRACKED_NAMESPACES`] are ever returned. That is not a convenience: the
/// screen is the secrets surface, and a general audit log read through it would be a way to read
/// every other feature's audit rows through a `secrets.audit` permission.
pub async fn list_audit(pool: &PgPool, filter: &AuditFilter) -> Result<Vec<AuditRow>> {
    // A crafted `?action=` cannot widen the read: anything outside the namespaces is dropped, and
    // a filter that ends up with nothing recognised narrows to an empty result rather than
    // falling back to "everything".
    let requested: Vec<String> = filter
        .actions
        .iter()
        .filter(|action| is_tracked(action))
        .cloned()
        .collect();

    let rows = if requested.is_empty() {
        if !filter.actions.is_empty() {
            return Ok(Vec::new());
        }
        // No filter: match the namespaces with `like`, which the action index can serve because
        // every value compared is a literal prefix -- no `any($1)` array and no per-name round trip.
        sqlx::query_as::<_, AuditRow>(
            "select id, action, target_type, target_id, actor_user_id, actor_type, \
                    ip_address::text as ip_address, metadata, request_id, lease_id, \
                    deployment_key_id, pipeline, created_at \
             from audit_log \
             where (action like 'secret.%' or action like 'deployment\\_key.%') \
               and ($1::uuid is null or target_id = $1::text) \
               and ($2::uuid is null or actor_user_id = $2) \
               and ($3::text is null or ip_address::text = $3) \
               and ($4::uuid is null or request_id = $4) \
               and ($5::timestamptz is null or created_at >= $5) \
             order by created_at desc, id desc \
             limit coalesce($6, 200)",
        )
        .bind(filter.secret_id)
        .bind(filter.actor_user_id)
        .bind(filter.address.as_deref())
        .bind(filter.request_id)
        .bind(filter.since)
        .bind(filter.limit)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as::<_, AuditRow>(
            "select id, action, target_type, target_id, actor_user_id, actor_type, \
                    ip_address::text as ip_address, metadata, request_id, lease_id, \
                    deployment_key_id, pipeline, created_at \
             from audit_log \
             where action = any($1) \
               and ($2::uuid is null or target_id = $2::text) \
               and ($3::uuid is null or actor_user_id = $3) \
               and ($4::text is null or ip_address::text = $4) \
               and ($5::uuid is null or request_id = $5) \
               and ($6::timestamptz is null or created_at >= $6) \
             order by created_at desc, id desc \
             limit coalesce($7, 200)",
        )
        .bind(&requested)
        .bind(filter.secret_id)
        .bind(filter.actor_user_id)
        .bind(filter.address.as_deref())
        .bind(filter.request_id)
        .bind(filter.since)
        .bind(filter.limit)
        .fetch_all(pool)
        .await?
    };
    Ok(rows)
}

/// The action names actually present in the trail, for the screen's filter control.
///
/// Derived from the rows rather than from a hand-written list, because a filter that offers an
/// action nothing writes is a dead control -- and, worse, a filter that *omits* an action that is
/// written hides evidence from the person reading the screen.
pub async fn distinct_actions(pool: &PgPool) -> Result<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "select distinct action from audit_log \
         where action like 'secret.%' or action like 'deployment\\_key.%' \
         order by action",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(action,)| action).collect())
}

/// Count the reveals of one secret by one actor in a window. The burst detector's input.
#[must_use]
pub fn window_start(at: OffsetDateTime) -> OffsetDateTime {
    at - Duration::hours(1)
}

/// Read the detector thresholds, falling back to the documented defaults.
///
/// The `coalesce` chain is what makes a database without 0032 answer sensibly: the table
/// missing is a query error, not a missing row, so the fallback lives at the call site.
pub async fn load_settings(pool: &PgPool) -> DetectorSettings {
    sqlx::query_as::<_, DetectorSettings>(
        "select business_hours_start::int as business_hours_start, \
                business_hours_end::int as business_hours_end, \
                reveal_burst_per_hour::int as reveal_burst_per_hour, \
                hard_rule_enforced, detect_new_network \
         from secret_audit_settings where id",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .unwrap_or_default()
}

/// The three counts the detectors need, from three point reads.
pub async fn reveal_counts(
    pool: &PgPool,
    secret_id: Uuid,
    actor_user_id: Option<Uuid>,
    at: OffsetDateTime,
) -> Result<RevealCounts> {
    let since = window_start(at);
    let actor = actor_user_id;
    let burst: i64 = sqlx::query_scalar(
        "select count(*)::int from audit_log \
         where action = $1 and target_id = $2::text \
           and actor_user_id is not distinct from $3 and created_at >= $4 and created_at <= $5",
    )
    .bind(actions::REVEALED)
    .bind(secret_id)
    .bind(actor)
    .bind(since)
    .bind(at)
    .fetch_one(pool)
    .await?;

    let prior: i64 = sqlx::query_scalar(
        "select count(*)::int from audit_log \
         where action = $1 and target_id = $2::text \
           and actor_user_id is not distinct from $3 and created_at <= $4",
    )
    .bind(actions::REVEALED)
    .bind(secret_id)
    .bind(actor)
    .bind(at)
    .fetch_one(pool)
    .await?;

    let addresses: i64 = sqlx::query_scalar(
        "select count(distinct ip_address)::int from audit_log \
         where action = $1 and target_id = $2::text \
           and actor_user_id is not distinct from $3 and ip_address is not null",
    )
    .bind(actions::REVEALED)
    .bind(secret_id)
    .bind(actor)
    .fetch_one(pool)
    .await?;

    Ok(RevealCounts {
        reveals_last_hour: burst,
        known_addresses: addresses,
        prior_reveals_by_actor: prior,
    })
}

/// Run the detectors for one reveal and persist whatever they raise.
///
/// The row is written even though nothing blocks: an anomaly that is never stored cannot be
/// acknowledged, and the acknowledge action is the only way an operator clears the flag.
pub async fn record_reveal_anomalies(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    observation: &RevealObservation,
) -> Result<Vec<AnomalyRow>> {
    let settings = load_settings(pool).await;
    let counts = reveal_counts(
        pool,
        observation.secret_id,
        observation.actor_user_id,
        observation.at,
    )
    .await
    .unwrap_or_default();
    let raised = Detectors::evaluate(&settings, observation, counts);
    let mut written = Vec::with_capacity(raised.patterns.len());
    for anomaly in raised.patterns {
        written.push(
            write_anomaly(
                pool,
                organization_id,
                observation,
                anomaly.pattern,
                &anomaly.detail,
            )
            .await?,
        );
    }
    Ok(written)
}

/// Insert one flag and read it back.
#[allow(clippy::too_many_arguments)]
pub async fn write_anomaly(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    observation: &RevealObservation,
    pattern: &'static str,
    detail: &Value,
) -> Result<AnomalyRow> {
    // The pattern is validated against this module's own list *and* the database constraint,
    // so a typo is caught in review rather than by a 500 at runtime.
    assert!(
        PATTERNS.contains(&pattern),
        "the anomaly pattern must be one this module knows"
    );
    let row = sqlx::query_as::<_, AnomalyRow>(
        "insert into secret_audit_anomalies \
             (organization_id, secret_id, pattern, actor_user_id, address, detail, request_id) \
         values ($1, $2, $3, $4, cast($5 as inet), $6, $7) \
         returning id, secret_id, pattern, severity, actor_user_id, \
                   address::text as address, detail, request_id, created_at, \
                   acknowledged_by, acknowledged_at, \
                   (select name from secrets where id = $2) as secret_name",
    )
    .bind(organization_id)
    .bind(observation.secret_id)
    .bind(pattern)
    .bind(observation.actor_user_id)
    .bind(observation.address.as_deref())
    .bind(detail)
    .bind(observation.request_id)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// Every flag, newest first. `unacknowledged_only` is what the screen defaults to.
pub async fn list_anomalies(
    pool: &PgPool,
    organization_id: Option<Uuid>,
    unacknowledged_only: bool,
    limit: i64,
) -> Result<Vec<AnomalyRow>> {
    let rows = sqlx::query_as::<_, AnomalyRow>(
        "select a.id, a.secret_id, a.pattern, a.severity, a.actor_user_id, \
                a.address::text as address, a.detail, a.request_id, a.created_at, \
                a.acknowledged_by, a.acknowledged_at, s.name as secret_name \
         from secret_audit_anomalies a left join secrets s on s.id = a.secret_id \
         where ($1::uuid is null or a.organization_id is null or a.organization_id = $1) \
           and (not $2 or a.acknowledged_at is null) \
         order by a.created_at desc, a.id desc limit $3",
    )
    .bind(organization_id)
    .bind(unacknowledged_only)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Acknowledge one flag. The result says whether a row was actually updated, so the screen can
/// say "already acknowledged" rather than pretending it changed something.
pub async fn acknowledge_anomaly(
    pool: &PgPool,
    id: i64,
    actor_user_id: Uuid,
    at: OffsetDateTime,
) -> Result<bool> {
    let updated: i64 = sqlx::query_scalar(
        "with changed as ( \
             update secret_audit_anomalies \
             set acknowledged_by = $2, acknowledged_at = $3 \
             where id = $1 and acknowledged_at is null \
             returning 1) \
         select count(*)::int from changed",
    )
    .bind(id)
    .bind(actor_user_id)
    .bind(at)
    .fetch_one(pool)
    .await?;
    Ok(updated > 0)
}

/// A SIEM-shaped record: the projection, with an explicit allowlist.
///
/// Every field here is metadata by construction. `metadata` is passed through because the
/// redaction helper of the request already ran *before* the row was stored, so the object
/// cannot contain a value — and the integration test asserts that with a grep for the fixture
/// value rather than trusting this comment.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SiemRecord {
    /// When the operation happened (RFC 3339, so a collector parses it without a format hint).
    pub timestamp: String,
    /// The action name.
    pub action: String,
    /// The target's kind.
    pub target_type: Option<String>,
    /// The target's id.
    pub target_id: Option<String>,
    /// Who acted.
    pub actor_user_id: Option<Uuid>,
    /// Which kind of actor.
    pub actor_type: String,
    /// Where from.
    pub source_ip: Option<String>,
    /// The organization, so a multi-tenant collector can route the record.
    pub organization_id: Option<Uuid>,
    /// The pipeline identity, when a machine spent the operation.
    pub pipeline: Option<String>,
    /// The request id, the join key a collector uses to correlate with the caller's own logs.
    pub request_id: Option<Uuid>,
    /// The structured detail, already redacted at write time.
    pub detail: Value,
}

/// Project an audit row into a SIEM record.
#[must_use]
pub fn siem_record(row: &AuditRow) -> SiemRecord {
    SiemRecord {
        timestamp: row
            .created_at
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
        action: row.action.clone(),
        target_type: row.target_type.clone(),
        target_id: row.target_id.clone(),
        actor_user_id: row.actor_user_id,
        actor_type: row.actor_type.clone(),
        source_ip: row.ip_address.clone(),
        organization_id: None,
        pipeline: row.pipeline.clone(),
        request_id: row.request_id,
        detail: row.metadata.clone(),
    }
}

/// The export, as newline-delimited JSON — one record per line, which is what every collector
/// in the request's list (webhook, JSON lines, syslog-shaped) can consume without a parser.
#[must_use]
pub fn siem_export(rows: &[AuditRow]) -> String {
    let mut out = String::new();
    for row in rows {
        if let Ok(line) = serde_json::to_string(&siem_record(row)) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

/// The local hour of `at` at a fixed UTC offset, in minutes east of UTC.
///
/// The detectors take the hour from the caller rather than deriving it, and this is the one
/// derivation there is. It exists as a function so the wrap arithmetic — the part that is easy to
/// get wrong by an hour — is tested once.
#[must_use]
pub fn local_hour(at: OffsetDateTime, offset_minutes: i32) -> u8 {
    let shifted = at + Duration::minutes(i64::from(offset_minutes));
    u8::try_from(shifted.hour()).unwrap_or(0)
}

/// The date of `at` at a fixed offset, for a business-hours rule that spans midnight.
#[must_use]
pub fn local_date(at: OffsetDateTime, offset_minutes: i32) -> Date {
    (at + Duration::minutes(i64::from(offset_minutes))).date()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(hour: u8) -> RevealObservation {
        RevealObservation {
            secret_id: Uuid::nil(),
            actor_user_id: Some(Uuid::nil()),
            address: Some("203.0.113.9".to_owned()),
            request_id: Uuid::nil(),
            at: OffsetDateTime::UNIX_EPOCH,
            local_hour: hour,
        }
    }

    #[test]
    fn business_hours_accept_a_wrapping_range() {
        let overnight = DetectorSettings {
            business_hours_start: 22,
            business_hours_end: 6,
            ..DetectorSettings::default()
        };
        for hour in [22, 23, 0, 1, 5] {
            assert!(
                overnight.within_business_hours(hour),
                "{hour} is inside a 22-06 shift"
            );
        }
        for hour in [6, 12, 18, 21] {
            assert!(
                !overnight.within_business_hours(hour),
                "{hour} is outside a 22-06 shift"
            );
        }
    }

    #[test]
    fn business_hours_that_wrap_all_the_way_round_means_always() {
        let all_day = DetectorSettings {
            business_hours_start: 0,
            business_hours_end: 0,
            ..DetectorSettings::default()
        };
        assert!((0..24).all(|h| all_day.within_business_hours(h)));
    }

    #[test]
    fn a_plain_range_still_works() {
        let day = DetectorSettings::default();
        assert!(day.within_business_hours(9));
        assert!(!day.within_business_hours(7));
        assert!(!day.within_business_hours(19));
    }

    #[test]
    fn the_off_hours_rule_fires_only_outside_the_window() {
        let settings = DetectorSettings::default();
        assert!(Detectors::off_hours_reveal(&settings, &observation(3)).is_some());
        assert!(Detectors::off_hours_reveal(&settings, &observation(12)).is_none());
    }

    #[test]
    fn a_burst_needs_the_count_to_reach_the_threshold() {
        let settings = DetectorSettings {
            reveal_burst_per_hour: 3,
            ..DetectorSettings::default()
        };
        assert!(
            Detectors::reveal_burst(&settings, &observation(12), 2).is_none(),
            "two reveals in an hour under a threshold of three is not a burst"
        );
        assert!(Detectors::reveal_burst(&settings, &observation(12), 3).is_some());
    }

    #[test]
    fn a_system_reveal_is_never_an_unfamiliar_principal() {
        // A runner holding a slot resolves it every few minutes. Flagging that is a row per
        // resolution, forever.
        let mut system = observation(12);
        system.actor_user_id = None;
        assert!(Detectors::unfamiliar_principal(&system, 0).is_none());
        assert!(Detectors::unfamiliar_principal(&observation(12), 0).is_some());
        assert!(Detectors::unfamiliar_principal(&observation(12), 1).is_none());
    }

    #[test]
    fn a_new_network_needs_an_address_and_no_history() {
        let settings = DetectorSettings::default();
        assert!(Detectors::new_network(&settings, &observation(12), 0).is_some());
        assert!(Detectors::new_network(&settings, &observation(12), 1).is_none());
        let off = DetectorSettings {
            detect_new_network: false,
            ..DetectorSettings::default()
        };
        assert!(Detectors::new_network(&off, &observation(12), 0).is_none());
    }

    #[test]
    fn evaluating_a_quiet_workday_reveal_raises_nothing() {
        let settings = DetectorSettings::default();
        let raised = Detectors::evaluate(
            &settings,
            &observation(11),
            RevealCounts {
                reveals_last_hour: 1,
                known_addresses: 3,
                prior_reveals_by_actor: 12,
            },
        );
        assert!(
            raised.patterns.is_empty(),
            "the normal case is no row at all"
        );
    }

    #[test]
    fn evaluating_a_suspicious_reveal_raises_every_rule_that_applies() {
        let settings = DetectorSettings::default();
        let raised = Detectors::evaluate(
            &settings,
            &observation(2),
            RevealCounts {
                reveals_last_hour: 40,
                known_addresses: 0,
                prior_reveals_by_actor: 0,
            },
        );
        let patterns: Vec<&str> = raised.patterns.iter().map(|a| a.pattern).collect();
        assert_eq!(
            patterns,
            vec![
                "off_hours_reveal",
                "reveal_burst",
                "new_network",
                "unfamiliar_principal"
            ],
            "all four rules are independent, so a maximally suspicious reveal hits all four"
        );
    }

    #[test]
    fn a_siem_record_carries_no_column_that_was_not_asked_for() {
        let row = AuditRow {
            id: 7,
            action: actions::REVEALED.to_owned(),
            target_type: Some("secret".to_owned()),
            target_id: Some("11111111-1111-1111-1111-111111111111".to_owned()),
            actor_user_id: Some(Uuid::nil()),
            actor_type: "user".to_owned(),
            ip_address: Some("198.51.100.7".to_owned()),
            // A detail object that, in production, has been through the redaction helper. The
            // test asserts the *shape* of the projection, not that the helper ran: the
            // integration suite is what greps for the fixture value.
            metadata: json!({ "note": "a human sentence" }),
            request_id: Some(Uuid::nil()),
            lease_id: Some(Uuid::nil()),
            deployment_key_id: None,
            pipeline: Some("github-actions".to_owned()),
            created_at: OffsetDateTime::UNIX_EPOCH,
        };
        let record = siem_record(&row);
        let serialized = serde_json::to_string(&record).expect("the record must serialize");
        // The lease id is deliberately *not* in the projection: a SIEM collector has no business
        // holding a lease handle, and a projection is the place to decide that once.
        assert!(!serialized.contains("lease_id"), "{serialized}");
        assert!(serialized.contains("github-actions"), "{serialized}");
        assert!(serialized.contains("198.51.100.7"), "{serialized}");
    }

    #[test]
    fn the_export_is_one_record_per_line() {
        let make = |id: i64| AuditRow {
            id,
            action: actions::DENIED.to_owned(),
            target_type: Some("secret".to_owned()),
            target_id: Some(Uuid::nil().to_string()),
            actor_user_id: Some(Uuid::nil()),
            actor_type: "user".to_owned(),
            ip_address: None,
            metadata: json!({}),
            request_id: Some(Uuid::nil()),
            lease_id: None,
            deployment_key_id: None,
            pipeline: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        };
        let export = siem_export(&[make(1), make(2)]);
        let lines: Vec<&str> = export.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            serde_json::from_str::<Value>(line).expect("every line is a complete object");
        }
    }

    #[test]
    fn the_local_hour_uses_the_offset_not_utc() {
        // 23:30 UTC on a +3 installation is 02:30 the next day, which is the difference between
        // "quiet overnight" and "off hours" for an operator in İğdır.
        let at = OffsetDateTime::UNIX_EPOCH + Duration::hours(23) + Duration::minutes(30);
        assert_eq!(local_hour(at, 0), 23);
        assert_eq!(local_hour(at, 180), 2);
        assert_eq!(local_hour(at, -300).to_string(), "18");
    }
}
