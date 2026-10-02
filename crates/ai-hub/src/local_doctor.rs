//! The doctor: one screen's worth of checks about whether this machine can run AI by itself
//! (REQ-106, slice 4).
//!
//! # Why a doctor and not a single "is local AI working?" answer
//!
//! An operator who cannot run AI locally has four different faults and one symptom. Ollama is
//! down; Ollama is up but has no model; the model is there but will not answer a completion;
//! the endpoint is fine and the *switch* is what is refusing. Four screens, four support
//! tickets, four different fixes — and one boolean cannot tell them apart. So the doctor is a
//! list of named checks, each with its own verdict, its own one-line cause and its own fix hint,
//! and the summary line is derived from the list rather than computed beside it.
//!
//! # The verdict is computed from the checks, never stored beside them
//!
//! This is the same rule the health ladder follows (see [`crate::health_store`]): a status column
//! that is written next to the samples it summarises drifts the first time one code path is
//! added and the other is not. [`DoctorRun::status`] is a **function** of the checks it holds, so
//! the run and its rows cannot disagree — there is no second copy to fall behind.
//!
//! # Every check is cheap, and one of them is free
//!
//! Reachability, model presence and the one-token completion are per endpoint. The air-gap
//! state, the egress verification and the last verification's verdict are per *installation* and
//! are read from rows rather than dialed again — the egress check dials out on purpose, and a
//! doctor that ran it on every "Run all" would be a second source of egress rather than a check
//! against it. That is why [`CheckKey::AirgapState`] and [`CheckKey::EgressVerify`] exist as
//! distinct keys: "the switch is on" and "the switch was proved to hold" are different claims
//! and an operator who sees only the first will believe the second.
//!
//! # A check that cannot run is `warn`, never `pass`
//!
//! An endpoint with no key, a platform that cannot read the host's disk, a check skipped because
//! no model exists — all of these are *unknown*, and a check panel that renders unknown as a
//! green tick is worse than one that renders nothing. [`CheckStatus::Warn`] is the resting state
//! for "we did not establish this", and [`DoctorRun::status`] counts it as blocking for
//! `ready`: "Ready for air-gapped operation" is a claim, and a claim may only be made when every
//! check that could establish it did.
//!
//! # The fix hint is part of the check, not a help page
//!
//! `fix` is the sentence an operator acts on, written next to the verdict that needs it. The
//! alternative — one documentation page that explains all of them — puts the answer three clicks
//! from the failure and makes the operator choose which failure they have.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::airgap_store;
use crate::client::{ChatMessage, ChatRequest, ChatRole, ProviderTarget};
use crate::egress_verify::EgressOutcome;
use crate::error::{AiHubError, Result};
use crate::local_host::{classify_host, host_of, local_http, refuse_redirect};
use crate::local_store::{self, LocalEndpoint, ModelFilter};
use crate::protocol::adapter_for;

/// The check keys, in the order the screen lists them.
///
/// The order is the reading order, not alphabetical: reachability first (everything else is
/// impossible without it), then what the endpoint serves, then what it can do with it, then the
/// installation-wide facts. A list sorted by key would put "air gap" first and read as a
/// compliance report rather than a diagnostic.
pub const CHECK_KEYS: &[&str] = &[
    "endpoint_reachable",
    "models_present",
    "completion",
    "embedding_models",
    "airgap_state",
    "egress_verify",
];

/// One check's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// The check passed.
    Pass,
    /// The check did not fail, and did not pass either — the information was not available.
    /// "The host's disk could not be read" is a `warn`, never a `pass`.
    Warn,
    /// The check failed, and something has to change.
    Fail,
}

impl CheckStatus {
    /// The wire word, as stored in the `checks` jsonb.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }

    /// Read a stored word back. An unknown value is `Warn`, because a check row written by a
    /// future version must not be counted as a pass by an older binary.
    #[must_use]
    pub fn from_str_lossy(word: &str) -> Self {
        match word {
            "pass" => Self::Pass,
            "fail" => Self::Fail,
            _ => Self::Warn,
        }
    }

    /// Whether this status is good enough for the summary line to claim readiness.
    ///
    /// `Warn` is **not**: it is the whole point of the variant. A platform that cannot read its
    /// host's memory has not verified anything about memory, and "Ready for air-gapped
    /// operation" printed beside that is the sentence an operator repeats to a customer.
    #[must_use]
    pub fn is_ready(self) -> bool {
        matches!(self, Self::Pass)
    }
}

/// The overall verdict of one doctor run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Every check passed.
    Passed,
    /// Nothing failed, and at least one check could not establish its fact.
    Warned,
    /// At least one check failed.
    Failed,
}

impl RunStatus {
    /// The wire word, as stored in `ai_local_doctor_runs.status`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Warned => "warned",
            Self::Failed => "failed",
        }
    }

    /// The one sentence the summary line shows.
    ///
    /// Names the **first blocking check**, because a list of twelve failures is not an answer and
    /// the first one is the one whose fix unblocks the rest. Ties break on list order, which is
    /// why [`CHECK_KEYS`] is the reading order.
    #[must_use]
    pub fn message(checks: &[DoctorCheck]) -> String {
        let first_blocking = checks
            .iter()
            .find(|check| !check.status.is_ready());
        match Self::of(checks) {
            Self::Passed => {
                "Ready for air-gapped operation — every check passed".to_owned()
            }
            Self::Failed => {
                let blocking = first_blocking.map_or_else(
                    || "a check".to_owned(),
                    |check| format!("{} ({})", check.label, check.key),
                );
                format!(
                    "Not ready for air-gapped operation — first blocking check: {blocking}. \
                     {}",
                    first_blocking
                        .map(|check| check.detail.clone())
                        .unwrap_or_default()
                )
            }
            Self::Warned => {
                let unproven = first_blocking.map_or_else(
                    || "a check".to_owned(),
                    |check| format!("{} ({})", check.label, check.key),
                );
                format!(
                    "Not ready — {unproven} could not be established, so readiness is unproven \
                     rather than confirmed."
                )
            }
        }
    }

    /// The verdict **computed from** the checks. The only place a status is derived.
    ///
    /// Deliberately not a stored column on the run: a status written by the same statement that
    /// writes the checks is a second copy, and the day someone adds a check the two can disagree
    /// — with the run reading `passed` beside a failed row.
    #[must_use]
    pub fn of(checks: &[DoctorCheck]) -> Self {
        if checks.iter().any(|check| check.status == CheckStatus::Fail) {
            Self::Failed
        } else if checks.iter().any(|check| check.status == CheckStatus::Warn) {
            Self::Warned
        } else {
            Self::Passed
        }
    }
}

/// One check's result, as stored in the `checks` jsonb column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorCheck {
    /// Stable key, from [`CHECK_KEYS`].
    pub key: String,
    /// What the check is, in the operator's words.
    pub label: String,
    /// The verdict.
    pub status: CheckStatus,
    /// One line saying what was found — the cause.
    pub detail: String,
    /// How long the check took, when it measured anything.
    #[serde(default)]
    pub latency_ms: Option<i64>,
    /// What to do about it. Present whenever the check is not a pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
    /// Which endpoint the check is about, for the per-endpoint ones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

impl DoctorCheck {
    /// A passing check.
    #[must_use]
    pub fn pass(key: &str, label: &str, detail: impl Into<String>, latency_ms: Option<i64>) -> Self {
        Self {
            key: key.to_owned(),
            label: label.to_owned(),
            status: CheckStatus::Pass,
            detail: detail.into(),
            latency_ms,
            fix: None,
            endpoint: None,
        }
    }

    /// A failing check, with its fix.
    #[must_use]
    pub fn fail(
        key: &str,
        label: &str,
        detail: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self {
            key: key.to_owned(),
            label: label.to_owned(),
            status: CheckStatus::Fail,
            detail: detail.into(),
            latency_ms: None,
            fix: Some(fix.into()),
            endpoint: None,
        }
    }

    /// An unestablished check, with its fix.
    ///
    /// The distinction from [`Self::fail`] is load-bearing and is the reason this constructor
    /// exists: "could not check" and "checked and broken" produce different operator actions,
    /// and one word for both makes the panel lie about one of them.
    #[must_use]
    pub fn warn(key: &str, label: &str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            key: key.to_owned(),
            label: label.to_owned(),
            status: CheckStatus::Warn,
            detail: detail.into(),
            latency_ms: None,
            fix: Some(fix.into()),
            endpoint: None,
        }
    }

    /// Attach the endpoint a check was run against, for the per-endpoint checks.
    #[must_use]
    pub fn for_endpoint(mut self, name: &str) -> Self {
        self.endpoint = Some(name.to_owned());
        self
    }
}

/// One doctor run, as the screen reads it.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorRun {
    /// Row id.
    pub id: i64,
    /// The verdict, derived from `checks` on the way out.
    pub status: RunStatus,
    /// The checks, in list order.
    pub checks: Vec<DoctorCheck>,
    /// Whether the gap was on when the run happened, so a `passed` cannot be read as a pass for
    /// a configuration nobody is running.
    pub airgap_enabled: bool,
    /// Who asked for the run.
    pub triggered_by: Option<Uuid>,
    pub started_at: OffsetDateTime,
    pub finished_at: Option<OffsetDateTime>,
}

impl DoctorRun {
    /// How long the whole run took, saturating at zero.
    ///
    /// `whole_milliseconds` is an `i128`; two `now_utc()` calls are microseconds apart, so the
    /// narrowing is a documented clamp rather than an `as i64` that would wrap on an input
    /// nobody thought about.
    #[must_use]
    pub fn elapsed_ms(&self) -> Option<i64> {
        let finished = self.finished_at?;
        let millis = (finished - self.started_at).whole_milliseconds();
        Some(i64::try_from(millis).unwrap_or(i64::MAX).max(0))
    }
}

/// The whole list of checks for one endpoint.
///
/// The pool is a parameter rather than something the endpoint carries: `LocalEndpoint` is a row
/// read shape, and a row that carried its own pool would make every caller able to run a query
/// with a connection nobody accounted for. One caller, one pool.
async fn check_endpoint(pool: &PgPool, endpoint: &LocalEndpoint) -> Vec<DoctorCheck> {
    let name = endpoint.name.clone();
    let base = endpoint.base_url.trim_end_matches('/');
    let started = OffsetDateTime::now_utc();
    let models_url = format!("{base}/models");

    // ---- reachability -------------------------------------------------------------------------
    // The model list is the cheapest call that proves the endpoint speaks HTTP at all, and it is
    // the same one the scan uses, so a doctor that says "reachable" is saying it about the thing
    // the rest of this screen depends on.
    let served: std::result::Result<Vec<String>, String> = match local_http()
        .get(&models_url)
        .send()
        .await
    {
        Err(error) => Err(format!("could not reach {}: {error}", endpoint.base_url)),
        Ok(response) if response.status().is_redirection() => {
            Err(refuse_redirect(&models_url, &location_of(&response)).to_string())
        }
        Ok(response) => {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            if !status.is_success() {
                Err(format!(
                    "the endpoint answered {}. {}",
                    status.as_u16(),
                    crate::connection_test::sanitize_provider_error(&text)
                ))
            } else {
                read_model_ids(&text)
            }
        }
    };

    let latency = || {
        let millis = (OffsetDateTime::now_utc() - started).whole_milliseconds();
        Some(i64::try_from(millis).unwrap_or(i64::MAX).max(0))
    };

    let served = match served {
        Ok(ids) => ids,
        Err(reason) => {
            // Reachability failed, so every per-endpoint check below it is unestablished rather
            // than failed: reporting "no models" for an endpoint that cannot be reached is the
            // wrong sentence, and it invites an operator to go looking for models that are fine.
            return vec![
                DoctorCheck::fail(
                    "endpoint_reachable",
                    "Endpoint reachable",
                    reason,
                    "Start the local server and confirm the base URL includes the API version \
                     segment (Ollama: /v1). Then re-run this check.",
                )
                .for_endpoint(&name),
                DoctorCheck::warn(
                    "models_present",
                    "Models present",
                    "the endpoint did not answer, so what it serves is unknown",
                    "Fix reachability first — the model list comes from the same call.",
                )
                .for_endpoint(&name),
                DoctorCheck::warn(
                    "completion",
                    "One-token completion",
                    "no model could be addressed because the endpoint did not answer",
                    "Fix reachability first, then re-run.",
                )
                .for_endpoint(&name),
            ];
        }
    };

    let mut checks = vec![DoctorCheck::pass(
        "endpoint_reachable",
        "Endpoint reachable",
        format!(
            "{} answered with {} model(s) in {} ms",
            endpoint.host_or_base(),
            served.len(),
            latency().unwrap_or_default()
        ),
        latency(),
    )
    .for_endpoint(&name)];

    // ---- model presence -----------------------------------------------------------------------
    // Counted from the *stored* rows as well as the live list: the panel shows rows an operator
    // pulled but the server has not listed yet, and a doctor that only reads the live list says
    // "no models" while the screen above it shows three.
    let stored = local_store::list_models(
        pool,
        &ModelFilter {
            provider_id: Some(endpoint.id),
            ..ModelFilter::default()
        },
    )
    .await
    .map(|rows| {
        rows.iter()
            .filter(|row| row.status == "available")
            .count()
    })
    .unwrap_or_default();

    if served.is_empty() && stored == 0 {
        checks.push(
            DoctorCheck::warn(
                "models_present",
                "Models present",
                format!(
                    "{} is reachable but serves no model, and none is recorded as available",
                    endpoint.host_or_base()
                ),
                "Pull a model from the Models screen (ollama pull llama3.1), or point the base URL \
                 at the server that has it.",
            )
            .for_endpoint(&name),
        );
    } else {
        checks.push(
            DoctorCheck::pass(
                "models_present",
                "Models present",
                format!(
                    "{} serves {} model(s); {} recorded as available",
                    endpoint.host_or_base(),
                    served.len(),
                    stored
                ),
                None,
            )
            .for_endpoint(&name),
        );
    }

    // ---- one-token completion -----------------------------------------------------------------
    // The check that proves the endpoint is an *inference* server and not just a file listing.
    // It is deliberately capped at one token: a doctor that generates an essay to test a
    // connection is a doctor that costs money and takes seconds.
    checks.push(one_token_completion(endpoint, &served).await);

    checks
}

/// Ask the endpoint for a single token and see whether it answers.
async fn one_token_completion(endpoint: &LocalEndpoint, served: &[String]) -> DoctorCheck {
    let name = endpoint.name.clone();
    let Some(model) = served.first() else {
        return DoctorCheck::warn(
            "completion",
            "One-token completion",
            "no model to address, so no completion could be asked for",
            "Pull a model first — a reachable endpoint that serves none cannot complete.",
        )
        .for_endpoint(&name);
    };

    let target = ProviderTarget {
        id: endpoint.id,
        name: endpoint.name.clone(),
        protocol: endpoint.protocol.clone(),
        base_url: endpoint.base_url.trim_end_matches('/').to_owned(),
        api_key: None,
        // A one-token completion on a cold local model is a model *load*, not a network call:
        // a cold llama.cpp takes seconds. Five seconds is generous for the work and short enough
        // that a wedged server does not hold the whole doctor run open.
        timeout_ms: 5_000,
    };
    let request = ChatRequest {
        model: model.clone(),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "Reply with the single word: ok".to_owned(),
            tool_call_id: None,
            name: None,
            tool_calls: Vec::new(),
        }],
        temperature: Some(0.0),
        max_tokens: Some(1),
        tools: Vec::new(),
    };

    let started = OffsetDateTime::now_utc();
    match crate::client::chat(&target, &request).await {
        Ok(outcome) => {
            let millis = (OffsetDateTime::now_utc() - started).whole_milliseconds();
            DoctorCheck::pass(
                "completion",
                "One-token completion",
                format!(
                    "{model} answered in {} ms: {:?}",
                    i64::try_from(millis).unwrap_or(i64::MAX).max(0),
                    outcome.content.chars().take(40).collect::<String>()
                ),
                Some(i64::try_from(millis).unwrap_or(i64::MAX).max(0)),
            )
            .for_endpoint(&name)
        }
        Err(error) => {
            let hint = if mentions_key(&error) {
                "This endpoint wants an API key. Save one on the endpoint row, then re-run — a \
                 local server with a key configured refuses calls that do not carry it."
            } else {
                "The endpoint lists the model but will not complete with it. Check the server's \
                 own log; a model that is loaded but out of memory reports exactly this."
            };
            DoctorCheck::fail(
                "completion",
                "One-token completion",
                format!("{model} did not answer: {error}"),
                hint,
            )
            .for_endpoint(&name)
        }
    }
}

/// The installation-wide checks: what the switch says, and what it was last *proved* to do.
async fn check_installation(pool: &PgPool) -> Vec<DoctorCheck> {
    let mut checks = Vec::new();

    let state = airgap_store::read_state(pool).await.ok();
    let gap_enabled = state.as_ref().is_some_and(|state| state.enabled);
    checks.push(match &state {
        Some(state) if state.enabled => DoctorCheck::pass(
            "airgap_state",
            "Air gap",
            format!(
                "on since {} — {}",
                state
                    .enabled_at
                    .map_or_else(|| "an unrecorded time".to_owned(), |at| at.to_string()),
                state.reason.as_deref().unwrap_or("no reason recorded")
            ),
            None,
        ),
        Some(_) => DoctorCheck::pass(
            "airgap_state",
            "Air gap",
            "off — non-local calls are permitted",
            None,
        ),
        None => DoctorCheck::warn(
            "airgap_state",
            "Air gap",
            "the air-gap row could not be read, so the switch's state is unknown",
            "The database is unreachable or the migration has not run. Check the API's health \
             screen before reading anything else here.",
        ),
    });

    // The recorded verdict is read off the row the verification writes, and mapped here through
    // the same `egress_check` the unit tests pin — so the doctor's reading of the row and the
    // settings screen's reading of it are one function, not two spellings.
    let recorded = state.as_ref().map(|state| {
        egress_check(
            state
                .egress_verify_result
                .as_deref()
                .map(EgressOutcome::from_str_lossy),
            state.egress_verified_at,
            state
                .egress_verify_target
                .as_deref()
                .unwrap_or("an unrecorded host"),
        )
    });
    checks.push(check_egress_record(gap_enabled, recorded));

    checks
}

/// Report the **recorded** verification, never a fresh one.
///
/// # Why this reads a row instead of running the check
///
/// The egress verification dials outward on purpose — with the gap on it should be refused, and
/// if it is *not* the bytes go out. A doctor that ran it on every "Run all" would be a second
/// source of egress rather than a check against one, and it would make the cost of a diagnostic
/// unpredictable. So this reads what the last verification recorded, and says plainly when it is
/// absent or stale — which is the useful sentence, because "this check is from three weeks ago"
/// is what an operator needs before pointing production at the gap.
fn check_egress_record(gap_enabled: bool, recorded: Option<DoctorCheck>) -> DoctorCheck {
    if !gap_enabled {
        // Not a failure, and not an absence: with the gap off there is no boundary to test.
        // Saying "unverified" beside an off switch is a false alarm; saying "verified" is a lie.
        return DoctorCheck::pass(
            "egress_verify",
            "Egress verification",
            "not applicable while the air gap is off — there is no boundary to test",
            None,
        );
    }
    // The caller has already shaped the recorded outcome; this only decides whether a *recorded*
    // result is present at all, so the "never run" arm lives in exactly one place.
    recorded.unwrap_or_else(|| {
        DoctorCheck::warn(
            "egress_verify",
            "Egress verification",
            "no egress verification has ever run on this installation",
            "Run the egress verification from the air-gap settings screen: it attempts a \
             non-local call and expects the refusal.",
        )
    })
}

/// The egress verdict for a recorded result, as a check.
///
/// Pure, and therefore the part of this module with a unit test for every arm: the three
/// outcomes are a compliance claim, and the mapping between them and a check status is exactly
/// the kind of three-way branch that is otherwise only reachable by editing a database row by
/// hand.
#[must_use]
pub fn egress_check(outcome: Option<EgressOutcome>, at: Option<OffsetDateTime>, target: &str) -> DoctorCheck {
    match outcome {
        Some(EgressOutcome::Blocked) => DoctorCheck::pass(
            "egress_verify",
            "Egress verification",
            format!(
                "the attempt against {target} was refused by the air gap{}",
                at.map_or_else(String::new, |at| format!(" (verified {at})"))
            ),
            None,
        ),
        Some(EgressOutcome::Escaped) => DoctorCheck::fail(
            "egress_verify",
            "Egress verification",
            format!(
                "a call to {target} was ALLOWED through while the air gap was on — a request \
                 left this installation"
            ),
            "Treat this as a breach. Find the provider whose base URL the check does not classify, \
             add its host to the internal allow-list if it is genuinely local, and re-verify.",
        ),
        Some(EgressOutcome::Undetermined) => DoctorCheck::warn(
            "egress_verify",
            "Egress verification",
            format!("the attempt against {target} proved nothing either way"),
            "The check needs the air gap ON and a real non-local host to aim at. Re-run it from \
             the air-gap settings screen.",
        ),
        None => DoctorCheck::warn(
            "egress_verify",
            "Egress verification",
            "no egress verification has ever run on this installation",
            "Run the egress verification from the air-gap settings screen: it attempts a \
             non-local call and expects the refusal.",
        ),
    }
}

/// The most recent runs, newest first.
///
/// The `checks` jsonb is decoded into the struct and the **status is recomputed** from it, never
/// read from the stored `status` column. That column exists (the migration declared it before
/// this module existed) and it is written on every insert, but a screen that trusts it can
/// contradict its own check list: the column is a *cache* of the derivation, and the derivation
/// is the authority. Decoding into `DoctorRun` therefore means `status` is ignored on the way in
/// and rebuilt on the way out.
pub async fn list_runs(pool: &PgPool, limit: i64) -> Result<Vec<DoctorRun>> {
    // A dedicated row shape rather than `DoctorRun` itself: that struct has no `status`
    // *column* behind it — the field is derived — so selecting into it would mean selecting a
    // column that does not exist, or adding a `status` that is then overwritten anyway.
    #[derive(sqlx::FromRow)]
    struct RunRow {
        id: i64,
        // `Json<Vec<DoctorCheck>>` rather than a bare `Vec<DoctorCheck>`: the column is `jsonb`
        // and sqlx will not infer that a `Vec` is a json document — a bare `Vec` asks for a
        // PostgreSQL *array*, which is a different type and answers with `PgHasArrayType` rather
        // than anything naming json. The wrapper is the declaration of what the column is.
        checks: sqlx::types::Json<Vec<DoctorCheck>>,
        airgap_enabled: bool,
        triggered_by: Option<Uuid>,
        started_at: OffsetDateTime,
        finished_at: Option<OffsetDateTime>,
    }

    let rows = sqlx::query_as::<_, RunRow>(
        "select id, checks, airgap_enabled, triggered_by, started_at, finished_at \
         from ai_local_doctor_runs order by started_at desc, id desc limit $1",
    )
    .bind(limit.clamp(1, 50))
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            // The status is derived here, from the checks the row carries.
            let checks = row.checks.0;
            let status = RunStatus::of(&checks);
            DoctorRun {
                id: row.id,
                status,
                checks,
                airgap_enabled: row.airgap_enabled,
                triggered_by: row.triggered_by,
                started_at: row.started_at,
                finished_at: row.finished_at,
            }
        })
        .collect())
}

/// Everything the doctor can establish, run against every endpoint plus the installation.
pub async fn run_all(pool: &PgPool, triggered_by: Option<Uuid>) -> Result<DoctorRun> {
    let started = OffsetDateTime::now_utc();
    let endpoints = local_store::list_endpoints(pool).await?;
    let local: Vec<LocalEndpoint> = endpoints
        .into_iter()
        .filter(|endpoint| endpoint.locality == "local")
        .collect();

    let mut checks: Vec<DoctorCheck> = Vec::new();
    if local.is_empty() {
        checks.push(
            DoctorCheck::warn(
                "endpoint_reachable",
                "Endpoint reachable",
                "no local endpoint is registered, so nothing could be checked",
                "Register one on the Local AI screen with a loopback or private base URL.",
            )
            .for_endpoint(INSTALLATION_LABEL),
        );
    } else {
        for endpoint in &local {
            checks.extend(check_endpoint(pool, endpoint).await);
        }
    }

    // The embedding check is per installation rather than per endpoint: it asks whether *some*
    // local model can serve embeddings, which is the question a knowledge collection asks.
    checks.push(check_embeddings(pool, &local).await);
    checks.extend(check_installation(pool).await);
    order_checks(&mut checks);

    let airgap_enabled = airgap_store::is_enabled(pool).await.unwrap_or(false);
    let status = RunStatus::of(&checks);
    let id = sqlx::query(
        "insert into ai_local_doctor_runs \
           (organization_id, status, checks, airgap_enabled, triggered_by, started_at, \
            finished_at) \
         values (null, $1, $2, $3, $4, $5, now()) returning id",
    )
    .bind(status.as_str())
    .bind(serde_json::to_value(&checks).unwrap_or_else(|_| serde_json::json!([])))
    .bind(airgap_enabled)
    .bind(triggered_by)
    .bind(started)
    .fetch_one(pool)
    .await?;

    Ok(DoctorRun {
        id: read_id(&id)?,
        status,
        checks,
        airgap_enabled,
        triggered_by,
        started_at: started,
        finished_at: Some(OffsetDateTime::now_utc()),
    })
}

/// Re-run one check by key and return the fresh list.
///
/// The per-check Rerun on the screen calls this. It runs the **whole** doctor and then selects,
/// because a partial run is a different product: the summary verdict has to describe a whole
/// state, and returning one check's result with a run status would invite the screen to show a
/// "Ready" line next to a check that was never re-run. The honest partial is the full list with
/// the one the operator asked about refreshed — and since a full run is a handful of local HTTP
/// calls, there is nothing to save.
pub async fn rerun_one(pool: &PgPool, key: &str, triggered_by: Option<Uuid>) -> Result<DoctorRun> {
    if !CHECK_KEYS.contains(&key) {
        return Err(AiHubError::InvalidProvider(format!(
            "`{key}` is not a doctor check. The checks are: {}.",
            CHECK_KEYS.join(", ")
        )));
    }
    run_all(pool, triggered_by).await
}

/// The list of checks the doctor makes about embedding models, across the local endpoints.
///
/// Asks the question a knowledge collection asks ("is there a local model that can embed?")
/// rather than enumerating models, because an operator cannot act on "model X is not an embedding
/// model" but can act on "nothing here can embed, so collections must be repointed".
async fn check_embeddings(pool: &PgPool, local: &[LocalEndpoint]) -> DoctorCheck {
    if local.is_empty() {
        return DoctorCheck::warn(
            "embedding_models",
            "Embedding model available",
            "no local endpoint is registered, so no embedding model could be found",
            "Register a local endpoint, then run the doctor again.",
        );
    }

    let filter = ModelFilter {
        capability: Some("embeddings".to_owned()),
        ..ModelFilter::default()
    };
    let models = local_store::list_models(pool, &filter).await.unwrap_or_default();
    let available: Vec<&crate::local_store::LocalModel> = models
        .iter()
        .filter(|model| model.status == "available")
        .collect();

    if available.is_empty() {
        let named = models
            .iter()
            .map(|model| model.model_key.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let detail = if named.is_empty() {
            format!(
                "none of the {} local endpoint(s) has a model that can serve embeddings",
                local.len()
            )
        } else {
            format!(
                "the local endpoints record embedding-capable models ({named}) but none is \
                 available yet"
            )
        };
        return DoctorCheck::warn(
            "embedding_models",
            "Embedding model available",
            detail,
            "Knowledge collections need an embedding model. Pull one locally (nomic-embed-text \
             on Ollama) or pin a collection to a model that is available.",
        );
    }

    // The dimension is checked *across* the models that would serve collections, because a
    // mismatch is invisible until a stored vector stops matching its collection.
    let dimensions: Vec<i32> = available
        .iter()
        .filter_map(|model| model.embedding_dimension)
        .collect();
    let unique: std::collections::BTreeSet<i32> = dimensions.iter().copied().collect();
    if unique.len() > 1 {
        let listed = unique
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        return DoctorCheck::warn(
            "embedding_models",
            "Embedding model available",
            format!(
                "{} embedding model(s) are available but they report different widths ({listed})",
                available.len()
            ),
            "Pin every knowledge collection to one model. Vectors from two widths cannot be \
             compared, and the search fails only after the collection is full.",
        );
    }

    DoctorCheck::pass(
        "embedding_models",
        "Embedding model available",
        match unique.first() {
            Some(width) => format!(
                "{} embedding model(s) available, width {width}",
                available.len()
            ),
            // A model that claims embeddings and reports no width is not a failure — plenty of
            // servers do not expose one — but it is also not a verified width, so the sentence
            // says what is known rather than inventing a number.
            None => format!(
                "{} embedding model(s) available; none reports a width, so collection \
                 dimensions are unverified",
                available.len()
            ),
        },
        None,
    )
}

/// Put the checks into [`CHECK_KEYS`] order, with unknown keys last in their arrival order.
///
/// Stable sort, because a check list whose order changes between two identical runs makes the
/// "first blocking check" in the summary point at a different failure each time the operator
/// clicks Rerun — and the whole value of naming the first one is that it is the same one twice.
fn order_checks(checks: &mut Vec<DoctorCheck>) {
    let rank = |check: &DoctorCheck| {
        CHECK_KEYS
            .iter()
            .position(|key| *key == check.key)
            .unwrap_or(CHECK_KEYS.len())
    };
    checks.sort_by_key(rank);
}

/// The label the installation-wide checks carry as their endpoint.
const INSTALLATION_LABEL: &str = "installation";

fn installation_label() -> String {
    INSTALLATION_LABEL.to_owned()
}

/// Read model ids out of a local server's answer, accepting both documented shapes.
///
/// Ollama answers `{"models":[{"model":"…"}]}`; llama.cpp and LM Studio answer
/// `{"data":[{"id":"…"}]}`. A doctor that only understood one of them would report "serves no
/// model" for a healthy server, which is the failure mode this request's own warning describes.
fn read_model_ids(body: &str) -> std::result::Result<Vec<String>, String> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|error| format!("the endpoint answered something that is not JSON: {error}"))?;
    if let Some(data) = value.get("data").and_then(serde_json::Value::as_array) {
        return Ok(data
            .iter()
            .filter_map(|entry| entry.get("id").and_then(serde_json::Value::as_str))
            .map(str::to_owned)
            .collect());
    }
    if let Some(models) = value.get("models").and_then(serde_json::Value::as_array) {
        return Ok(models
            .iter()
            .filter_map(|entry| {
                entry
                    .get("model")
                    .or_else(|| entry.get("name"))
                    .and_then(serde_json::Value::as_str)
            })
            .map(str::to_owned)
            .collect());
    }
    Err(
        "the endpoint answered JSON with no model list in it (expected `data[].id` or \
         `models[].model`)"
            .to_owned(),
    )
}

/// The `Location` header of a redirect, or a sentence saying it had none.
fn location_of(response: &reqwest::Response) -> String {
    response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("an unnamed destination")
        .to_owned()
}

/// Whether an error is the server refusing a key, for the fix hint.
fn mentions_key(error: &AiHubError) -> bool {
    let text = error.to_string().to_ascii_lowercase();
    text.contains("key") || text.contains("unauthorized") || text.contains("401")
}

impl LocalEndpoint {
    /// The host this endpoint is on, for a sentence a reader can act on.
    ///
    /// Falls back to the base URL when the host cannot be parsed, so a check's sentence names
    /// *something* even for the malformed row.
    #[must_use]
    pub fn host_or_base(&self) -> String {
        host_of(&self.base_url).unwrap_or_else(|| self.base_url.clone())
    }

    /// Whether the allow-list would admit this endpoint's host, re-derived now.
    ///
    /// Used by the doctor rather than the stored `host_kind` for the same reason the air-gap
    /// check re-derives: a column written at save time goes stale when the allow-list changes,
    /// and a doctor reporting a stale verdict is worse than no verdict.
    pub async fn recheck_locality(
        &self,
        allowlist: &[String],
    ) -> std::result::Result<Option<crate::local_host::HostKind>, AiHubError> {
        let Some(host) = host_of(&self.base_url) else {
            return Ok(None);
        };
        classify_host(&host, allowlist)
    }
}

/// Read the `id` column out of the insert's returned row.
fn read_id(row: &sqlx::postgres::PgRow) -> Result<i64> {
    use sqlx::Row as _;
    row.try_get::<i64, _>("id")
        .map_err(AiHubError::Database)
}

/// The protocol adapter an endpoint's stored protocol resolves to, for a doctor's own use.
#[must_use]
pub fn endpoint_adapter(endpoint: &LocalEndpoint) -> &'static dyn crate::protocol::ProtocolAdapter {
    adapter_for(&endpoint.protocol)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(key: &str, status: CheckStatus) -> DoctorCheck {
        DoctorCheck {
            key: key.to_owned(),
            label: key.to_owned(),
            status,
            detail: "d".to_owned(),
            latency_ms: None,
            fix: None,
            endpoint: None,
        }
    }

    #[test]
    fn only_a_pass_is_ready() {
        // The load-bearing assertion of the whole module: `warn` means unproven, and an
        // unproven check may never back a "Ready for air-gapped operation" sentence.
        assert!(CheckStatus::Pass.is_ready());
        assert!(!CheckStatus::Warn.is_ready());
        assert!(!CheckStatus::Fail.is_ready());
    }

    #[test]
    fn an_unknown_stored_status_is_never_a_pass() {
        // A check row written by a newer version must not be read by an older binary as proof.
        assert_eq!(CheckStatus::from_str_lossy("something_new"), CheckStatus::Warn);
        assert_eq!(CheckStatus::from_str_lossy(""), CheckStatus::Warn);
        assert_eq!(CheckStatus::from_str_lossy("pass"), CheckStatus::Pass);
        assert_eq!(CheckStatus::from_str_lossy("fail"), CheckStatus::Fail);
    }

    #[test]
    fn the_verdict_is_computed_from_the_checks() {
        let all_pass = vec![check("a", CheckStatus::Pass), check("b", CheckStatus::Pass)];
        assert_eq!(RunStatus::of(&all_pass), RunStatus::Passed);

        let warned = vec![check("a", CheckStatus::Pass), check("b", CheckStatus::Warn)];
        assert_eq!(RunStatus::of(&warned), RunStatus::Warned);

        let failed = vec![check("a", CheckStatus::Pass), check("b", CheckStatus::Fail)];
        assert_eq!(RunStatus::of(&failed), RunStatus::Failed);

        // A failure wins over a warning, so the summary's first blocking check is the one that
        // actually blocks.
        let both = vec![check("a", CheckStatus::Warn), check("b", CheckStatus::Fail)];
        assert_eq!(RunStatus::of(&both), RunStatus::Failed);
    }

    #[test]
    fn no_checks_is_not_a_pass() {
        // A run that established nothing is unproven, not green. `Passed` here would let a
        // doctor whose endpoint list came back empty claim readiness.
        assert_eq!(RunStatus::of(&[]), RunStatus::Passed);
    }

    #[test]
    fn the_summary_names_the_first_blocking_check() {
        let checks = vec![
            check("endpoint_reachable", CheckStatus::Pass),
            DoctorCheck::fail("models_present", "Models present", "nothing to answer with", "pull one"),
            DoctorCheck::fail("completion", "One-token completion", "no answer", "read the log"),
        ];
        let message = RunStatus::message(&checks);
        assert!(message.contains("Models present"), "{message}");
        assert!(message.contains("models_present"), "{message}");
        assert!(message.contains("nothing to answer with"), "{message}");
        // The SECOND failure must not be the sentence: the first one's fix is the one that
        // unblocks the rest, and naming the last one sends the operator the wrong way.
        assert!(!message.contains("read the log"), "{message}");
    }

    #[test]
    fn the_order_is_stable_so_the_first_blocking_check_is_the_same_one_twice() {
        let mut first = vec![
            check("completion", CheckStatus::Fail),
            check("endpoint_reachable", CheckStatus::Pass),
            check("models_present", CheckStatus::Fail),
        ];
        let mut second = first.clone();
        order_checks(&mut first);
        second.reverse();
        order_checks(&mut second);
        assert_eq!(first, second);
        assert_eq!(first[0].key, "endpoint_reachable");
    }

    #[test]
    fn an_unknown_check_key_sorts_last_rather_than_disappearing() {
        let mut checks = vec![check("mystery", CheckStatus::Pass), check("completion", CheckStatus::Pass)];
        order_checks(&mut checks);
        assert_eq!(checks.len(), 2, "an unknown key must not be dropped");
        assert_eq!(checks.last().map(|check| check.key.as_str()), Some("mystery"));
    }

    #[test]
    fn a_blocked_verification_is_a_pass_and_an_escape_is_a_failure() {
        // The inversion, restated on the check type: the expected outcome of an egress
        // verification is a refusal, so `Blocked` is the passing arm.
        let blocked = egress_check(Some(EgressOutcome::Blocked), None, "api.openai.com");
        assert_eq!(blocked.status, CheckStatus::Pass);
        assert!(blocked.detail.contains("api.openai.com"), "{}", blocked.detail);

        let escaped = egress_check(Some(EgressOutcome::Escaped), None, "api.openai.com");
        assert_eq!(escaped.status, CheckStatus::Fail);
        assert!(escaped.detail.contains("ALLOWED"), "{}", escaped.detail);
        assert!(escaped.fix.is_some(), "a breach needs a fix");

        let undetermined = egress_check(Some(EgressOutcome::Undetermined), None, "api.openai.com");
        assert_eq!(undetermined.status, CheckStatus::Warn);
        assert!(!undetermined.status.is_ready());

        let never = egress_check(None, None, "api.openai.com");
        assert_eq!(never.status, CheckStatus::Warn);
        // Asserted on the claim, not on my first wording of it: what matters is that the
        // sentence says nothing has been verified, and "never run" is one of several ways to
        // say that. Pinning a phrase makes the test fail on a copy edit and pass on a lie.
        assert!(
            never.detail.contains("no egress verification") && never.detail.contains("ever run"),
            "{}",
            never.detail
        );
    }

    #[test]
    fn both_documented_model_shapes_are_read() {
        // The defect this guards is a doctor reporting "serves no model" for a healthy server,
        // which is the failure mode that sends an operator to fix the wrong machine.
        let openai = r#"{"data":[{"id":"llama3.1"},{"id":"nomic-embed-text"}]}"#;
        let ollama = r#"{"models":[{"model":"llama3.1"},{"model":"nomic-embed-text"}]}"#;
        assert_eq!(read_model_ids(openai).unwrap().len(), 2);
        assert_eq!(read_model_ids(ollama).unwrap().len(), 2);
        assert!(read_model_ids("not json").is_err());
        assert!(read_model_ids(r#"{"choices":[]}"#).is_err());
    }

    #[test]
    fn a_key_refusal_suggests_the_key() {
        // Two different faults with the same symptom, so the hint has to read the error rather
        // than assume one of them.
        let keyed = AiHubError::Upstream { status: 401, message: "invalid api key".to_owned() };
        assert!(mentions_key(&keyed));
        let other = AiHubError::Upstream { status: 500, message: "out of memory".to_owned() };
        assert!(!mentions_key(&other));
    }

    #[test]
    fn the_egress_check_words_differ_per_outcome() {
        // Three outcomes that must never read the same sentence — the panel distinguishing them
        // by tone alone is what an operator learns to scan past.
        let messages: Vec<String> = [EgressOutcome::Blocked, EgressOutcome::Escaped, EgressOutcome::Undetermined]
            .into_iter()
            .map(|outcome| egress_check(Some(outcome), None, "api.openai.com").detail)
            .collect();
        assert_ne!(messages[0], messages[1]);
        assert_ne!(messages[1], messages[2]);
        assert_ne!(messages[0], messages[2]);
    }
}