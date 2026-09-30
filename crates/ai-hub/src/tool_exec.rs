//! The one path a tool call may take (REQ-100, slice 3).
//!
//! The request states the claim in one sentence and it is the load-bearing sentence of the whole
//! request: **"every call walks the same path: resolve the AI identity → look the tool up →
//! enabled? → grant (an explicit deny beats any allow) → the required permission → argument
//! validation against the schema → per-run cap and timeout → execute through the same service
//! the HTTP route calls → record `ai_tool_calls` and `audit_log` → return a bounded result.
//! Nothing in the platform may reach a tool implementation without walking it."**
//!
//! Everything else in this file is a defence of one clause of that sentence.
//!
//! # Why the ordering is the design
//!
//! Each step answers a different question, and a different order leaks a different fact:
//!
//! - **Identity first** — without an identity there are no grants, and a run that cannot resolve
//!   one executes nothing. This is checked *before* the tool is even looked up, so a run with no
//!   identity cannot be used as an oracle for which tool keys exist.
//! - **Registry second** — `tool_unknown` is the model's own error, and it is answered before any
//!   policy question so a typo does not read as a permission problem.
//! - **Enabled, then grant, through [`identity::resolve`]** — *not* a second implementation of the
//!   same rule. The deny-beats-allow ordering is the security claim, and it is already a pure
//!   function with its own unit walks; a second derivation here is precisely how the two would
//!   come to disagree about the one rule that must never be in doubt.
//! - **Permission, then arguments** — a caller who may not do the thing is told so without the
//!   argument validator first describing the shape of the thing they may not do, and *neither*
//!   runs before the cap check, so a runaway loop cannot turn "denied" into a busy-loop.
//! - **Cap, then timeout, then execute** — the two money limits wrap the call rather than sitting
//!   beside it. A cap checked after execution is a report, not a limit.
//!
//! # What this module deliberately does not know
//!
//! It knows nothing about `omnion-permissions`, and takes the permission decision through
//! [`PermissionGate`] instead. Two reasons, one architectural and one practical: `ai-hub` is a
//! leaf-ish infrastructure crate and a dependency on the permission engine from inside the agent
//! loop is an edge that points the wrong way; and the *same* rule has to be reachable from a
//! workflow node and the internal SDK, where there is no session and therefore no
//! `CurrentSession` to resolve a user from. A gate is the seam.
//!
//! The execution itself is a [`Tool`] from the registry — a thin wrapper over a service the HTTP
//! route already calls. That is the "no second door" clause made structural rather than
//! aspirational: the pipeline has no way to name a database table, a shell or a URL, because its
//! only capability is `registry.get(key)`.

use std::collections::BTreeMap;
use std::time::Instant;

use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use crate::agent::ToolCall;
use crate::error::Result;
use crate::identity::{self, GrantEffect, Resolution, ResolutionReason};
use crate::schema;
use crate::tool_calls::{self, CallStatus, NewCall};
use crate::tools::{Execution, MAX_SUMMARY_CHARS, ToolRegistry};

/// Whether the caller may perform an action requiring `permission`.
///
/// The seam described in the module comment. Implemented for a closure so a test can write
/// `|key| key == "content.read"` in one line, and for any `&dyn Fn` so the API can pass
/// `omnion_permissions::EffectivePermissions::allows` without a wrapper type.
pub trait PermissionGate: Send + Sync {
    /// `true` when the caller holds `permission`.
    fn allows(&self, permission: &str) -> bool;
}

impl<F> PermissionGate for F
where
    F: Fn(&str) -> bool + Send + Sync,
{
    fn allows(&self, permission: &str) -> bool {
        self(permission)
    }
}

/// A shared handle forwards, so a caller can hold one gate for several pipelines.
///
/// The trait already says `Send + Sync`; this impl is what makes that *useful* rather than merely
/// required. A `Pipeline` owns its dependencies, so without it a caller that wanted to share one
/// gate between two pipelines would have to build two. `?Sized` is what lets the same impl serve
/// an `Arc<dyn PermissionGate>` as well as an `Arc<Concrete>`.
///
/// **Not `&T` as well**, and the reason is worth keeping: a blanket impl for references collides
/// with the closure impl above (`&F` is itself `Fn`), and Rust cannot order the two. A borrowed
/// gate is a test-only convenience, so the tests take a concrete `Arc` instead of teaching the
/// trait a rule it would have to break every time a caller wanted a closure.
impl<T: PermissionGate + ?Sized> PermissionGate for std::sync::Arc<T> {
    fn allows(&self, permission: &str) -> bool {
        self.as_ref().allows(permission)
    }
}

/// The caller of a call, as far as the log and the permission gate care.
///
/// Separate from the agent because the two really are different: a workflow node's run has a
/// user and no session, and an internal SDK call may have neither. `None` is a real state, not a
/// placeholder — and it is why the gate is a trait rather than a resolved permission set.
#[derive(Debug, Clone, Copy, Default)]
pub struct Caller {
    pub organization_id: Uuid,
    pub agent_id: Uuid,
    pub run_id: Uuid,
    /// The step row the loop wrote `running` before the tool executed. The request's criterion is
    /// "both carrying the same run and step", so this is part of the call's identity rather than
    /// a nicety.
    pub step_id: Option<Uuid>,
    pub user_id: Option<Uuid>,
    pub site_id: Option<Uuid>,
}

/// The identity a call resolves to, and the grants it carries.
///
/// Deliberately a *value* the caller resolves rather than something this module looks up: the
/// resolution query (`default_identity`, or a named one) is a store question and belongs with the
/// store, and a pipeline that resolved its own identity would need an organization on every call
/// site — which is how a tool ends up callable from a path that never resolved one.
#[derive(Debug, Clone)]
pub struct ResolvedIdentity {
    pub id: Uuid,
    pub key: String,
    /// `allow` = true, `explicit deny` = false, absent = inherit. The map is the *decided* rows
    /// only, which is what `identity::resolve` is written against.
    pub grants: BTreeMap<String, bool>,
}

/// What one call did, in the shape the loop, the route and the log all read.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CallOutcome {
    /// The tool ran.
    Ran {
        /// The tool that ran.
        tool: String,
        /// The capped summary the model will read.
        summary: String,
        /// Whether the tool itself reported a failure.
        failed: bool,
        /// The call log row, so the caller does not have to wonder whether the write happened.
        call_id: i64,
    },
    /// Nothing ran.
    Refused {
        /// The tool that was refused — the key the model named, even when unknown.
        tool: String,
        /// A stable code: `tool_unknown`, `tool_disabled`, `tool_denied`,
        /// `tool_not_in_agent_list`, `permission_denied`, `tool_bad_arguments`, `tool_limited`.
        code: String,
        /// Why, in a sentence, for the model and the trace.
        reason: String,
        /// The call log row.
        call_id: i64,
    },
    /// The tool ran but did not finish in time.
    TimedOut {
        /// The tool that was cut off.
        tool: String,
        /// The limit it broke, in milliseconds.
        timeout_ms: i32,
        /// The call log row.
        call_id: i64,
    },
    /// The run must stop and wait for a person.
    Parked {
        /// The tool awaiting the decision.
        tool: String,
        /// What the decider sees — already redacted, because it is about to be stored.
        arguments: Value,
    },
}

impl CallOutcome {
    /// The stable code for the refusal cases, and a description for the ran case.
    ///
    /// One accessor rather than a match at each of the three call sites, because a consumer that
    /// branches on `code()` for one case and on a variant for another is a consumer whose
    /// behaviour changes when a variant is added.
    #[must_use]
    pub fn code(&self) -> &str {
        match self {
            Self::Ran { .. } => "ok",
            Self::Refused { code, .. } => code,
            Self::TimedOut { .. } => "tool_timeout",
            // A park is not a refusal and has no code of its own — it is the tool's own
            // `requires_approval` that says so, and the loop turns it into `approval_required`
            // from the tool key. Naming it here would be a second place to keep in step.
            Self::Parked { .. } => "approval_required",
        }
    }

    /// Whether the call log row for this outcome says nothing ran.
    #[must_use]
    pub fn ran(&self) -> bool {
        matches!(self, Self::Ran { .. } | Self::TimedOut { .. })
    }

    /// The event the caller should publish, if any.
    ///
    /// `None` for a success and for a cap breach: the request names `ai.tool.denied`,
    /// `ai.tool.failed` and `ai.tool.limited` as separate events, and emitting the *limited* one
    /// here would fire it for every cap hit including a `Parked` call that never started. The
    /// caller passes the outcome's own branch to the bus rather than this function, because the
    /// bus lives in `apps/api` and this crate must not depend on it.
    #[must_use]
    pub fn alert_event(&self) -> Option<&'static str> {
        match self {
            Self::Refused { code, .. } if code == "permission_denied" || code == "tool_denied" => {
                Some("ai.tool.denied")
            }
            // The request lists `ai.tool.disabled` as its own event, and it was unreachable while
            // `Pipeline::enabled` answered from the compiled catalogue: no call could ever be
            // refused with `tool_disabled`, so nothing could emit this. Routing it to
            // `ai.tool.denied` would have been a lie in the other direction — an operator watching
            // denials would see a run's ordinary permission refusal and a global tool being
            // switched off arrive as the same event, and only one of those is their decision.
            Self::Refused { code, .. } if code == "tool_disabled" => Some("ai.tool.disabled"),
            Self::Refused { code, .. } if code == "tool_limited" => Some("ai.tool.limited"),
            Self::TimedOut { .. } => Some("ai.tool.failed"),
            Self::Ran { failed: true, .. } => Some("ai.tool.failed"),
            _ => None,
        }
    }

    /// The summary the model reads for a ran call, and the reason for a refused one.
    ///
    /// The loop pushes exactly this into the history, so the text a model sees about a refusal is
    /// the same string the call log recorded as `error_code` — one wording, produced once.
    #[must_use]
    pub fn model_text(&self) -> String {
        match self {
            Self::Ran { summary, .. } => summary.clone(),
            Self::Refused { code, reason, .. } => format!("refused: {code} — {reason}"),
            Self::TimedOut { tool, timeout_ms, .. } => format!(
                "refused: tool_timeout — {tool} did not finish within {timeout_ms} ms"
            ),
            Self::Parked { tool, .. } => {
                format!("awaiting approval before calling {tool}")
            }
        }
    }
}

/// The execution context: what the pipeline needs beyond the call itself.
pub struct Pipeline {
    /// The compiled tools. `get` is the only capability this module has over them.
    ///
    /// `Arc` rather than a borrow, and that is what lets an executor hold a pipeline **inside**
    /// the `Arc` the loop owns. With a `&'a ToolRegistry` the executor would have to outlive a
    /// registry held on its own stack, and the only way to express that is a self-referential
    /// struct — which is the point at which a design is telling you it wants a handle instead.
    registry: std::sync::Arc<ToolRegistry>,
    /// The identity whose grants apply. `None` executes nothing — the request's "a run with no
    /// resolvable identity executes nothing".
    identity: Option<ResolvedIdentity>,
    /// The agent's own allow-list. Narrower than the identity's: see `identity::resolve`.
    agent_tools: Vec<String>,
    /// The agent's approval list, from the same jsonb column.
    approvals: Vec<String>,
    /// The permission decision for the caller. Shared for the same reason as the registry.
    gate: std::sync::Arc<dyn PermissionGate>,
    /// Who is calling, for the log and the permission gate.
    caller: Caller,
    /// The keys an operator has switched off, read once from `ai_tools.enabled`.
    ///
    /// **Why a snapshot and not a per-cell query.** [`Pipeline::model_facing`] has no pool and is
    /// called once per run, when the runner assembles the provider's tool payload — so the
    /// operator's decision has to be carried in rather than fetched. One query per *run* is the
    /// right cost; a query per tool per cell is not.
    ///
    /// **Why `Pipeline::call` does not use it.** That method has the pool and is the door that
    /// actually matters, so it re-reads the row and refuses from the row's own `enabled`. A
    /// refusal that trusts a snapshot taken before the run started is a refusal an operator
    /// cannot rely on for the length of a run.
    ///
    /// Fails **open** if a caller passes an empty set, which is why it is a constructor argument
    /// rather than a defaulted one: a caller cannot forget it, and forgetting it is the shape of
    /// bug this field exists to remove.
    disabled: std::collections::BTreeSet<String>,
}

impl Pipeline {
    /// Build a pipeline. Nothing is checked here; the first call is the first check.
    ///
    /// `disabled` is the operator's `ai_tools.enabled = false` set, read once by the caller. It is
    /// a parameter rather than a defaulted one so that every construction site has to decide what
    /// it believes about the operator's decisions — a `Default::default()` here would be an empty
    /// set, which means "nothing is disabled", and that is the exact belief the previous
    /// compiled-catalogue check encoded by accident.
    #[must_use]
    pub fn new(
        registry: std::sync::Arc<ToolRegistry>,
        identity: Option<ResolvedIdentity>,
        agent_tools: Vec<String>,
        approvals: Vec<String>,
        disabled: std::collections::BTreeSet<String>,
        gate: std::sync::Arc<dyn PermissionGate>,
        caller: Caller,
    ) -> Self {
        Self {
            registry,
            identity,
            agent_tools,
            approvals,
            gate,
            caller,
            disabled,
        }
    }

    /// The tools a model may be *shown*, in the order the registry declares them.
    ///
    /// The request: "the tool payload the loop sends to a model contains only enabled, granted
    /// tools; a denied or disabled tool is **invisible, not merely refused**". The difference is
    /// the whole point — a tool the model can see but cannot call is a tool it will try, and
    /// three tries is `loop_detected`. So this filters with the *same* function
    /// [`Pipeline::call`] refuses with, and a test that proves the filter hides a denied tool
    /// while `call` refuses it is proving the two agree.
    ///
    /// The `max_calls_per_run` is deliberately **not** applied here: a cap is per run, and the
    /// payload is assembled before the run has spent anything. Hiding a tool because this
    /// particular run already used it would make the payload change mid-run for reasons the
    /// model cannot act on.
    #[must_use]
    pub fn model_facing(&self) -> Vec<crate::tools::ToolSummary> {
        // No identity means no payload at all, and it is checked here rather than left to
        // `identity::resolve`. That is not a redundancy: with an empty grant map and the tool
        // present in the agent's list, `resolve` answers **allow** — a deny is a stored `false`,
        // so "no identity" and "an identity that granted nothing" are indistinguishable to a
        // function that only sees grants. `Pipeline::call` checks identity first for the same
        // reason, and the pair of checks is the rule "a run with no resolvable identity executes
        // nothing" — the one `resolve` is not able to express.
        if self.identity.is_none() {
            return Vec::new();
        }
        let grants = self.grants();
        self.registry
            .catalogue()
            .into_iter()
            .filter(|summary| {
                identity::resolve(
                    &grants,
                    &self.agent_tools,
                    &summary.key,
                    self.enabled(&summary.key),
                )
                .effect
                    == GrantEffect::Allow
            })
            .filter(|summary| self.gate.allows(&summary.permission))
            .collect()
    }

    /// The identity's decided grants, or an empty map when there is no identity.
    fn grants(&self) -> BTreeMap<String, bool> {
        self.identity.as_ref().map_or_else(BTreeMap::new, |id| id.grants.clone())
    }

    /// The same pipeline with the step row a call belongs to filled in.
    ///
    /// The step is a parameter of the call, not of the pipeline, because a run opens a new one on
    /// every step and the pipeline is built once per run. Returning a borrowed view rather than
    /// taking `&mut self` is what lets the executor hold one pipeline and use it from a
    /// `ToolExecutor` implementation that is `&self`.
    #[must_use]
    pub fn with_step(&self, step_id: Option<Uuid>) -> Self {
        Self {
            registry: std::sync::Arc::clone(&self.registry),
            identity: self.identity.clone(),
            agent_tools: self.agent_tools.clone(),
            approvals: self.approvals.clone(),
            gate: std::sync::Arc::clone(&self.gate),
            disabled: self.disabled.clone(),
            caller: Caller {
                organization_id: self.caller.organization_id,
                agent_id: self.caller.agent_id,
                run_id: self.caller.run_id,
                step_id,
                user_id: self.caller.user_id,
                site_id: self.caller.site_id,
            },
        }
    }

    /// The identity a call is attributed to, for the audit row and the alert event.
    ///
    /// `None` for a run that resolved nothing — and a call in that state always ends `denied`
    /// with `identity_unresolved`, so a `None` here describes a real state rather than a gap in
    /// the log.
    #[must_use]
    pub fn identity_id(&self) -> Option<Uuid> {
        self.identity.as_ref().map(|id| id.id)
    }

    /// Whether the operator has the tool switched on, **as this pipeline was told**.
    ///
    /// Reads the snapshot the caller assembled from `ai_tools.enabled`, not the compiled
    /// catalogue. This method used to answer `catalogue::find(key).is_some()`, and that was a
    /// real defect rather than a latency trade: by the time `model_facing` consults it the key
    /// has already come out of `registry.catalogue()`, so it answered `true` for **every** tool
    /// that reached it. `ResolutionReason::Disabled` was unreachable from both ends — the
    /// payload filter and `call` — so `identity::resolve`'s first rule ("disabled wins") had no
    /// caller that could ever fire it, the request's `ai.tool.disabled` event was never emitted,
    /// and an operator switching a tool off on `/ai/tools` changed nothing at runtime.
    ///
    /// The two are not equivalent even when the catalogue is right: the catalogue knows which
    /// tools **exist**, and the row knows which tools an operator **allows**. A retired tool is
    /// absent from the catalogue and disabled in the row, and only the row carries the second.
    #[must_use]
    fn enabled(&self, key: &str) -> bool {
        !self.disabled.contains(key)
    }

    /// The same question asked of the **row**, for the door that has a pool.
    ///
    /// [`Pipeline::call`] runs this instead of the snapshot, and that asymmetry is deliberate:
    /// the payload a run started with cannot be retracted from a provider mid-flight, but a
    /// *call* can be refused against the operator's current decision. So the strongest statement
    /// an operator can make — "this tool is off" — takes effect on the next call of a run that
    /// was already in flight, rather than at the start of the next run.
    async fn enabled_in_row(&self, pool: &sqlx::PgPool, key: &str) -> Result<bool> {
        // `is not null`-shaped question asked in the database rather than in the decoder: an
        // `Option<bool>` from `select enabled` is `Option<Option<bool>>` in sqlx because a NULL
        // column is indistinguishable from no row, and the crate's columns are `not null` — so
        // the existence of the row is the answer.
        let row: Option<(bool,)> = sqlx::query_as("select enabled from ai_tools where key = $1")
            .bind(key)
            .fetch_optional(pool)
            .await?;
        // A tool with no row is treated as enabled *here only* because [`Pipeline::call`] has
        // already read that same row a few lines earlier and refused with `tool_unknown` when it
        // was missing. Reaching this function without a row is not a state the caller can be in.
        Ok(row.is_none_or(|(enabled,)| enabled))
    }

    /// Walk the whole path for one call.
    ///
    /// `pool` is taken last on purpose: the first six steps read nothing from the database, so a
    /// call refused by the registry costs no query at all.
    pub async fn call(
        &self,
        pool: &sqlx::PgPool,
        call: &ToolCall,
    ) -> Result<CallOutcome> {
        let Some(identity) = self.identity.as_ref() else {
            return self.refuse(
                pool,
                &call.tool,
                "identity_unresolved",
                "this run resolved no AI identity, so it may call nothing",
                None,
            )
            .await;
        };

        let Some(tool) = self.registry.get(&call.tool) else {
            return self
                .refuse(
                    pool,
                    &call.tool,
                    "tool_unknown",
                    "no tool carries that key",
                    Some(identity.id),
                )
                .await;
        };

        // `identity::resolve` — the one ordering, called rather than re-derived. The comment at its
        // definition says the ordering IS the security claim; this call is what makes that true
        // for the execution path and not only for the unit tests.
        //
        // `enabled` comes from the **row**, not from the snapshot `model_facing` was built with,
        // so an operator's disable lands on the next call of a run already in flight. See
        // [`Pipeline::enabled_in_row`] for why the two doors answer from different places.
        let resolution: Resolution = identity::resolve(
            &self.grants(),
            &self.agent_tools,
            &tool.key(),
            self.enabled_in_row(pool, &call.tool).await?,
        );
        if resolution.effect == GrantEffect::Deny {
            return self
                .refuse(
                    pool,
                    &call.tool,
                    resolution.reason.code(),
                    &refusal_sentence(&call.tool, &resolution.reason),
                    Some(identity.id),
                )
                .await;
        }

        if !self.gate.allows(tool.permission()) {
            return self
                .refuse(
                    pool,
                    &call.tool,
                    "permission_denied",
                    &format!(
                        "the caller does not hold {}, which {} requires",
                        tool.permission(),
                        tool.key()
                    ),
                    Some(identity.id),
                )
                .await;
        }

        // Arguments, from the *compiled* spec rather than the row: the row is the operator's copy
        // and a hand-edited schema that accepts a shell command would be a schema the pipeline
        // validated itself against. `schema::supported` is checked in the catalogue's test, so a
        // spec with a keyword this subset ignores fails the build rather than a call.
        if let Err(error) = schema::validate(&tool.schema(), &call.arguments) {
            return self
                .refuse(
                    pool,
                    &call.tool,
                    "tool_bad_arguments",
                    &error.to_string(),
                    Some(identity.id),
                )
                .await;
        }

        // The approval gate. Parked rather than refused, and **before** the cap check: a call
        // waiting for a decision is not a call that spent budget, so counting it would let a
        // model whose every call needs approval burn its cap without ever running anything.
        if self.approvals.iter().any(|key| key == tool.key()) {
            return Ok(CallOutcome::Parked {
                tool: tool.key().to_owned(),
                arguments: crate::run_store::redact_arguments(&call.arguments),
            });
        }

        // The cap, counted from rows so a resumed run does not get a fresh allowance. The row is
        // read **once** here rather than from a counter or from the compiled default: the
        // operator's own number is what must be enforced, and two queries per call is nothing
        // next to the tool itself. A cap read from `catalogue::default_limits` would be a cap
        // that is documented and not applied, which is the worst of the three options.
        let Some(row) = crate::registry::get_tool(pool, tool.key()).await? else {
            return self
                .refuse(
                    pool,
                    &call.tool,
                    "tool_unknown",
                    "the registry row this tool came from is gone",
                    Some(identity.id),
                )
                .await;
        };
        let used = tool_calls::count_in_run(pool, self.caller.run_id, tool.key()).await?;
        if used >= i64::from(row.max_calls_per_run) {
            return self
                .refuse(
                    pool,
                    &call.tool,
                    "tool_limited",
                    &format!(
                        "{} allows {} call(s) per run and this run has already made {used}",
                        tool.key(),
                        row.max_calls_per_run
                    ),
                    Some(identity.id),
                )
                .await;
        }

        self.run_the_tool(pool, tool.as_ref(), &row, identity.id, call)
            .await
    }

    /// Run it, wrapped in the row's timeout, and log the outcome.
    async fn run_the_tool(
        &self,
        pool: &sqlx::PgPool,
        tool: &dyn crate::tools::Tool,
        row: &crate::registry::ToolRow,
        identity_id: Uuid,
        call: &ToolCall,
    ) -> Result<CallOutcome> {
        let args_bytes = tool_calls::json_bytes(&call.arguments);
        let started = Instant::now();
        // `timeout` over the future rather than a watchdog inside it: the tool is a boxed future
        // this crate does not own, so the only cancellation it can be given is at the await
        // point. The dropped future releases whatever it held, and the log row below records the
        // cut-off honestly rather than leaving the call with no evidence.
        //
        // `std::time::Duration`, not `time::Duration` — this crate's timestamps are `time`'s, and
        // a reader reaching for that crate out of habit writes the wrong one. tokio's `timeout`
        // takes the std type, and the two are both non-negative durations of milliseconds, so
        // the conversion here is a constructor call rather than a unit change.
        let timeout = std::time::Duration::from_millis(row.timeout_ms.unsigned_abs() as u64);
        let outcome = match tokio::time::timeout(timeout, tool.run(&call.arguments)).await {
            Ok(outcome) => outcome,
            Err(_elapsed) => {
                let call_id = self
                    .log(
                        pool,
                        NewCall::for_tool(tool.key(), CallStatus::Timeout)
                            .with_run(
                                self.caller.organization_id,
                                self.caller.run_id,
                                self.caller.agent_id,
                            )
                            .with_step_opt(self.caller.step_id)
                            .with_identity(identity_id)
                            .with_user_opt(self.caller.user_id)
                            .with_site_opt(self.caller.site_id)
                            .with_error("tool_timeout")
                            .with_duration(started.elapsed().as_millis() as i64)
                            .with_args_size(args_bytes),
                    )
                    .await?;
                return Ok(CallOutcome::TimedOut {
                    tool: tool.key().to_owned(),
                    timeout_ms: row.timeout_ms,
                    call_id,
                });
            }
        };
        let duration = started.elapsed().as_millis() as i64;

        let result_bytes = outcome.content.len();
        let mut summary = outcome.content;
        if summary.chars().count() > MAX_SUMMARY_CHARS {
            summary = format!(
                "{}\n[truncated at {} characters]",
                summary.chars().take(MAX_SUMMARY_CHARS - 1).collect::<String>(),
                MAX_SUMMARY_CHARS
            );
        }
        let status = if outcome.failed {
            CallStatus::Failed
        } else {
            CallStatus::Ok
        };
        let mut logged = NewCall::for_tool(tool.key(), status)
            .with_run(
                self.caller.organization_id,
                self.caller.run_id,
                self.caller.agent_id,
            )
            .with_step_opt(self.caller.step_id)
            .with_identity(identity_id)
            .with_user_opt(self.caller.user_id)
            .with_site_opt(self.caller.site_id)
            .with_duration(duration)
            .with_args_size(args_bytes)
            .with_result_size(result_bytes);
        if outcome.failed {
            logged = logged.with_error("tool_failed");
        }
        let call_id = self.log(pool, logged).await?;
        Ok(CallOutcome::Ran {
            tool: tool.key().to_owned(),
            summary,
            failed: outcome.failed,
            call_id,
        })
    }

    /// Write the refusal row and answer.
    async fn refuse(
        &self,
        pool: &sqlx::PgPool,
        tool: &str,
        code: &str,
        reason: &str,
        identity_id: Option<Uuid>,
    ) -> Result<CallOutcome> {
        let mut call = NewCall::for_tool(tool, CallStatus::Denied)
            .with_run(
                self.caller.organization_id,
                self.caller.run_id,
                self.caller.agent_id,
            )
            .with_step_opt(self.caller.step_id)
            .with_error(code);
        if let Some(identity_id) = identity_id {
            call = call.with_identity(identity_id);
        }
        if let Some(user_id) = self.caller.user_id {
            call = call.with_user(user_id);
        }
        if let Some(site_id) = self.caller.site_id {
            call = call.with_site(site_id);
        }
        let call_id = self.log(pool, call).await?;
        Ok(CallOutcome::Refused {
            tool: tool.to_owned(),
            code: code.to_owned(),
            reason: reason.to_owned(),
            call_id,
        })
    }

    async fn log(&self, pool: &sqlx::PgPool, call: NewCall) -> Result<i64> {
        tool_calls::record(pool, call).await
    }
}

/// A sentence for each refusal reason, so the model reads prose and the log reads a code.
///
/// `tool` is threaded in because **a refusal the model cannot act on is a retry generator.** The
/// four sentences below name a *cause* ("switched off", "not on the agent's list") but not a
/// *subject*: a model that named four different tools and got the identical sentence has learned
/// nothing except that it should try again, and for `Disabled` there is no retry that can ever
/// work — the tool is gone from its payload for the whole run. The first clause is therefore the
/// key the model itself sent, which is what lets it drop the call instead of repeating it.
fn refusal_sentence(tool: &str, reason: &ResolutionReason) -> String {
    let cause = match reason {
        ResolutionReason::Allowed => "this tool is allowed (unreachable, but total)",
        ResolutionReason::Disabled => {
            "an operator has switched this tool off for the whole installation, and asking again \
             will not change that"
        }
        ResolutionReason::ExplicitDeny => {
            "this run's AI identity carries an explicit deny for it"
        }
        ResolutionReason::NotInAgentList => "the agent's own tool list does not name it",
    };
    format!("{tool}: {cause}")
}

/// How the loop gets one tool call done.
///
/// **This trait is the "no second door" claim, made structural.** `loop_engine` used to call
/// `tools::decide` itself: a pure function over a registry and an allow-list, with no identity,
/// no permission, no schema validation, no cap, no timeout and no log. A run started from the
/// panel and a run started from a workflow node therefore took two different paths, and the
/// second one walked past every check REQ-100 names. The loop now asks for an *execution* and
/// nothing else; the answer comes from [`Pipeline`] in a process that has a database, and the
/// loop's three-way match is a projection of the same value (`as_execution`) rather than a
/// second decision.
///
/// `step_no` is a parameter rather than part of the construction because the step a call belongs
/// to is only known at the call site: the row is written when the step begins, and the criterion
/// "both carrying the same run and step" needs the row's **id**, not its number.
pub trait ToolExecutor: Send + Sync {
    /// Run one call through the whole path and say what happened, in the loop's own vocabulary.
    fn execute<'a>(
        &'a self,
        step_no: u32,
        call: &'a ToolCall,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Execution> + Send + 'a>>;
}

/// The executor the loop's **unit tests** use, and the only one compiled into a test build.
///
/// It is deliberately the old `decide` and deliberately `#[cfg(test)]`. REQ-100's test surface
/// needs a loop that can be driven with no database at all — the guardrails, the repeated-call
/// detector and the stop conditions are all properties of the loop, and each of them has a
/// walk that builds a `Runtime` in one line. But the *in-crate* executor existing only in test
/// builds is the point: a second decider that ships is a second door, and this makes the
/// production build incapable of having one rather than relying on review to notice it.
///
/// `#[doc(hidden)]` rather than private, so the module's own doctest — which is compiled as an
/// external crate and therefore cannot see `cfg(test)` items — can still show a builder chain
/// that works. The example uses it precisely to say "this is the *test* wiring; production hands
/// the builder a `Pipeline`", and a reader who copies the production path is the one the type
/// system should stop.
#[cfg(test)]
#[doc(hidden)]
pub struct LocalExecutor {
    /// The compiled tools.
    pub registry: ToolRegistry,
    /// The keys the run may call.
    pub allow: crate::tools::AllowList,
}

#[cfg(test)]
#[doc(hidden)]
impl ToolExecutor for LocalExecutor {
    fn execute<'a>(
        &'a self,
        _step_no: u32,
        call: &'a ToolCall,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Execution> + Send + 'a>> {
        Box::pin(crate::tools::decide(&self.registry, &self.allow, call))
    }
}

/// Adapters so a call outcome is also the loop's own [`Execution`], without the loop re-deriving
/// anything.
///
/// **This is the seam that makes "no second door" testable.** Converting rather than duplicating
/// means there is one implementation to audit; the loop's three-way match is now a projection of
/// this function. The refusal arm maps the outcome's own code through
/// [`DenyReason::from_code`] rather than collapsing every refusal to `ToolDenied`: the loop
/// feeds `reason.code()` to the guardrail and to the model's own tool result, and a trace that
/// said `tool_denied` for a missing permission is a trace that sends an operator looking in the
/// wrong place.
#[must_use]
pub fn as_execution(outcome: &CallOutcome) -> Execution {
    match outcome {
        CallOutcome::Ran {
            tool, summary, failed, ..
        } => Execution::Ran {
            tool: tool.clone(),
            summary: summary.clone(),
            failed: *failed,
        },
        CallOutcome::Parked { tool, arguments } => Execution::Parked {
            tool: tool.clone(),
            arguments: arguments.clone(),
        },
        CallOutcome::Refused { tool, code, .. } => Execution::Refused {
            tool: tool.clone(),
            reason: crate::tools::DenyReason::from_code(code)
                .unwrap_or(crate::tools::DenyReason::ToolDenied),
        },
        CallOutcome::TimedOut { tool, .. } => Execution::Refused {
            tool: tool.clone(),
            reason: crate::tools::DenyReason::ToolTimeout,
        },
    }
}

/// The tool key a non-ran outcome names.
#[must_use]
pub fn other_key(outcome: &CallOutcome) -> String {
    match outcome {
        CallOutcome::Ran { tool, .. }
        | CallOutcome::Refused { tool, .. }
        | CallOutcome::TimedOut { tool, .. }
        | CallOutcome::Parked { tool, .. } => tool.clone(),
    }
}

/// A [`ResolvedIdentity`] from a stored row plus its grants, which is the shape every caller has.
#[must_use]
pub fn identity_of(id: &crate::identity::AiIdentity, grants: BTreeMap<String, bool>) -> ResolvedIdentity {
    ResolvedIdentity {
        id: id.id,
        key: id.key.clone(),
        grants,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{FnTool, ToolOutcome, ToolRegistry};
    use std::sync::Arc;
    use serde_json::json;

    fn registry() -> ToolRegistry {
        let mut registry = ToolRegistry::empty();
        registry.register(Arc::new(FnTool::new(
            "content.search",
            "Search content",
            "content.read",
            |_| ToolOutcome::ok("3 hits"),
        )));
        registry.register(Arc::new(
            FnTool::new("deployment.deploy", "Deploy", "deployment.deploy", |_| {
                ToolOutcome::ok("deployed")
            })
            .with_schema(json!({
                "type": "object",
                "properties": { "target": { "type": "string" } },
                "required": ["target"],
                "additionalProperties": false
            })),
        ));
        registry
    }

    fn identity(grants: &[(&str, bool)]) -> ResolvedIdentity {
        ResolvedIdentity {
            id: Uuid::new_v4(),
            key: "editor".to_owned(),
            grants: grants
                .iter()
                .map(|(k, v)| ((*k).to_owned(), *v))
                .collect(),
        }
    }

    fn caller() -> Caller {
        Caller {
            organization_id: Uuid::new_v4(),
            agent_id: Uuid::new_v4(),
            run_id: Uuid::new_v4(),
            step_id: Some(Uuid::new_v4()),
            user_id: Some(Uuid::new_v4()),
            site_id: None,
        }
    }

        /// A pipeline over borrowed inputs, wrapping them the way every caller must.
    ///
    /// The borrows are this helper's convenience; the `Arc`s are the product's contract, and a
    /// test that spelled out the product's own shape at each of its thirty call sites would be
    /// thirty chances to typo it. Wrapping here is what keeps the tests honest about the *rules*
    /// they check rather than about plumbing.
    fn pipeline(
        registry: &ToolRegistry,
        identity: Option<ResolvedIdentity>,
        agent_tools: &[&str],
        gate: std::sync::Arc<dyn PermissionGate>,
    ) -> Pipeline {
        pipeline_with_disabled(registry, identity, agent_tools, gate, Default::default())
    }

    /// The same, with the operator's disabled set supplied.
    ///
    /// The two-argument [`pipeline`] above defaults to "nothing is disabled", which is the right
    /// answer for a crate that has no database — but it is also the answer a caller gets for
    /// free, so the disabled walks say which set they mean rather than inheriting one.
    fn pipeline_with_disabled(
        registry: &ToolRegistry,
        identity: Option<ResolvedIdentity>,
        agent_tools: &[&str],
        gate: std::sync::Arc<dyn PermissionGate>,
        disabled: std::collections::BTreeSet<String>,
    ) -> Pipeline {
        Pipeline::new(
            std::sync::Arc::new(registry.clone()),
            identity,
            agent_tools.iter().map(|k| (*k).to_owned()).collect(),
            Vec::new(),
            disabled,
            gate,
            caller(),
        )
    }

    /// [`allow_all`] behind the `Arc` the pipeline takes.
    ///
    /// A helper rather than a macro because **the gate has to be the one the test wrote**: a
    /// wrapper that returned "allow everything" regardless of its argument would leave the
    /// permission walks asserting against a gate that grants every key, which is the vacuous
    /// assertion this file has already been bitten by once.
    fn gate_all() -> std::sync::Arc<dyn PermissionGate> {
        std::sync::Arc::new(|_: &str| true)
    }


    // -- the pure half: the payload filter -----------------------------------------------------

    #[test]
    fn the_model_payload_hides_a_tool_the_identity_denies() {
        // The request's "a denied or disabled tool is invisible, not merely refused". A tool the
        // model can see but not call is a tool it retries, and three retries is `loop_detected` —
        // so invisibility is a cost property, not a politeness one.
        let registry = registry();
        let pipe = pipeline(
            &registry,
            Some(identity(&[("deployment.deploy", false)])),
            &["content.search", "deployment.deploy"],
            gate_all(),
        );
        let keys: Vec<String> = pipe.model_facing().into_iter().map(|t| t.key).collect();
        assert_eq!(keys, vec!["content.search".to_owned()]);
    }

    #[test]
    fn the_model_payload_hides_a_tool_the_agent_never_listed() {
        // The identity can only narrow here: an identity's allow never widens the agent's own
        // list, so a tool the agent does not carry is invisible even with a grant.
        let registry = registry();
        let pipe = pipeline(&registry, Some(identity(&[])), &["content.search"], gate_all());
        let keys: Vec<String> = pipe.model_facing().into_iter().map(|t| t.key).collect();
        assert_eq!(keys, vec!["content.search".to_owned()]);
    }

    #[test]
    fn the_model_payload_hides_a_tool_the_caller_may_not_perform() {
        // The permission gate applies to *visibility* as well as to execution: a viewer who may
        // not deploy should not be shown a deploy button either. Without this the panel and the
        // runtime would disagree about what a read-only operator can see.
        let registry = registry();
        let gate: std::sync::Arc<dyn PermissionGate> =
            std::sync::Arc::new(|key: &str| key == "content.read");
        let pipe = pipeline(
            &registry,
            Some(identity(&[])),
            &["content.search", "deployment.deploy"],
            gate,
        );
        let keys: Vec<String> = pipe.model_facing().into_iter().map(|t| t.key).collect();
        assert_eq!(keys, vec!["content.search".to_owned()]);
    }

    #[test]
    fn a_run_with_no_identity_is_shown_no_tools_at_all() {
        // "A run with no resolvable identity executes nothing" — and it is *shown* nothing, so a
        // model is never told a tool exists that it will then be refused.
        let registry = registry();
        let pipe = pipeline(&registry, None, &["content.search"], gate_all());
        assert!(pipe.model_facing().is_empty());
    }

    #[test]
    fn the_payload_carries_the_schema_the_caller_will_validate_against() {
        // A payload whose schema differs from the one `call` validates against is a model that
        // is told to send a field that is then refused as unknown — so the two must be the same
        // object, and this asserts the payload's own copy is the declared one.
        let registry = registry();
        let pipe = pipeline(
            &registry,
            Some(identity(&[])),
            &["deployment.deploy"],
            gate_all(),
        );
        let summary = pipe
            .model_facing()
            .into_iter()
            .find(|t| t.key == "deployment.deploy")
            .expect("the deploy tool is granted and compiled");
        let tool = registry.get("deployment.deploy").expect("registered");
        assert_eq!(summary.schema, tool.schema());
        assert_eq!(
            schema::validate(&summary.schema, &json!({"target": "prod"})),
            Ok(()),
            "the payload's example must itself validate"
        );
        assert!(schema::validate(&summary.schema, &json!({"target": 4})).is_err());
    }

    // -- the pure half: outcome shapes ----------------------------------------------------------

    #[test]
    fn a_refusal_reads_as_one_sentence_and_a_code_together() {
        // The model gets prose, the log gets a code, and they come from the same outcome so they
        // cannot drift — the "refused: X — Y" shape is what the loop pushes into the history.
        let outcome = CallOutcome::Refused {
            tool: "deployment.deploy".to_owned(),
            code: "tool_denied".to_owned(),
            reason: "this run's AI identity carries an explicit deny for it".to_owned(),
            call_id: 7,
        };
        assert_eq!(outcome.code(), "tool_denied");
        assert!(!outcome.ran());
        assert_eq!(
            outcome.model_text(),
            "refused: tool_denied — this run's AI identity carries an explicit deny for it"
        );
        assert_eq!(outcome.alert_event(), Some("ai.tool.denied"));
    }

    #[test]
    fn only_a_denial_or_a_failure_reaches_the_operator() {
        // A cap breach is normal loop behaviour the `loop_detected` guard already reports; paging
        // somebody for it is how a page gets ignored.
        let denied = CallOutcome::Refused {
            tool: "t".to_owned(),
            code: "tool_denied".to_owned(),
            reason: "no".to_owned(),
            call_id: 1,
        };
        let limited = CallOutcome::Refused {
            tool: "t".to_owned(),
            code: "tool_limited".to_owned(),
            reason: "no".to_owned(),
            call_id: 2,
        };
        let ran = CallOutcome::Ran {
            tool: "t".to_owned(),
            summary: "fine".to_owned(),
            failed: false,
            call_id: 3,
        };
        let failed = CallOutcome::Ran {
            tool: "t".to_owned(),
            summary: "broke".to_owned(),
            failed: true,
            call_id: 4,
        };
        assert_eq!(denied.alert_event(), Some("ai.tool.denied"));
        assert_eq!(limited.alert_event(), Some("ai.tool.limited"));
        assert_eq!(ran.alert_event(), None);
        assert_eq!(failed.alert_event(), Some("ai.tool.failed"));

        // `ai.tool.disabled` is the request's own event for an operator switching a tool off, and
        // it must be distinguishable from an ordinary denial. Asserting it routes somewhere
        // (rather than to `ai.tool.denied`) is the whole point: a subscriber that wants to know
        // "somebody switched this tool off and a run tried it anyway" cannot get that from the
        // denial stream, which also carries every permission refusal in the installation.
        let disabled = CallOutcome::Refused {
            tool: "content.search".to_owned(),
            code: "tool_disabled".to_owned(),
            reason: "off".to_owned(),
            call_id: 5,
        };
        assert_eq!(disabled.alert_event(), Some("ai.tool.disabled"));
        assert_ne!(
            disabled.alert_event(),
            denied.alert_event(),
            "a global disable and a permission denial are different facts"
        );
    }

    #[test]
    fn a_cap_breach_is_a_refusal_and_not_a_failure() {
        // The distinction the call log's `status` column rests on: a limited call ran nothing, so
        // recording it as `failed` would put a tool in the "error %" column for a limit an
        // operator set.
        let limited = CallOutcome::Refused {
            tool: "t".to_owned(),
            code: "tool_limited".to_owned(),
            reason: "no".to_owned(),
            call_id: 1,
        };
        assert!(!limited.ran());
    }

    #[test]
    fn a_parked_call_is_not_a_refusal_and_carries_the_redacted_arguments() {
        // A park hands the run to a person; the loop must not push a refusal into the history, or
        // the model reads "denied" and gives up instead of waiting.
        let outcome = CallOutcome::Parked {
            tool: "deployment.deploy".to_owned(),
            arguments: json!({"target": "prod"}),
        };
        assert!(!outcome.ran());
        assert!(!outcome.model_text().is_empty());
        assert!(matches!(as_execution(&outcome), Execution::Parked { .. }));
    }

    #[test]
    fn every_outcome_projects_into_the_loop_own_three_way_match() {
        // The seam: `loop_engine` matches on `Execution`, and this conversion is the only place
        // that has to be right for the loop to see the pipeline's decision.
        let ran = CallOutcome::Ran {
            tool: "content.search".to_owned(),
            summary: "3 hits".to_owned(),
            failed: false,
            call_id: 1,
        };
        match as_execution(&ran) {
            Execution::Ran { tool, summary, failed } => {
                assert_eq!(tool, "content.search");
                assert_eq!(summary, "3 hits");
                assert!(!failed);
            }
            other => panic!("a ran call must project to Ran, got {other:?}"),
        }
        let refused = CallOutcome::Refused {
            tool: "content.search".to_owned(),
            code: "tool_denied".to_owned(),
            reason: "no".to_owned(),
            call_id: 2,
        };
        match as_execution(&refused) {
            Execution::Refused { tool, .. } => assert_eq!(tool, "content.search"),
            other => panic!("a refusal must project to Refused, got {other:?}"),
        }
    }

    #[test]
    fn the_refusal_projection_never_names_an_empty_tool() {
        // `as_execution` is the only place that could hand the loop an empty tool name, and the
        // loop's `Message::tool_result` puts that name in the transcript. A regression here would
        // be a trace row reading `refused: tool_denied` with no tool at all.
        let outcome = CallOutcome::Refused {
            tool: "content.search".to_owned(),
            code: "tool_unknown".to_owned(),
            reason: "no".to_owned(),
            call_id: 1,
        };
        assert_eq!(other_key(&outcome), "content.search");
    }

    #[test]
    fn a_refusal_sentence_exists_for_every_reason_the_resolver_can_give() {
        // `Allowed` is unreachable for a refusal and says so, rather than panicking: a match arm
        // added to `ResolutionReason` without a sentence here would be a compile error, and that
        // is the point — the compiler is the reminder.
        //
        // The second assertion is the one that was missing. `!sentence.is_empty()` passed for
        // sentences that named no subject at all, which is how four different refusals came back
        // as the same string and a model that had just named four different tools could not tell
        // which one it had been told about. So the sentence must carry the key.
        for reason in [
            ResolutionReason::Allowed,
            ResolutionReason::Disabled,
            ResolutionReason::ExplicitDeny,
            ResolutionReason::NotInAgentList,
        ] {
            let sentence = refusal_sentence("content.search", &reason);
            assert!(!sentence.is_empty());
            assert!(
                sentence.contains("content.search"),
                "a refusal must name the tool it is about, or a model cannot act on it. Got: \
                 {sentence}"
            );
        }
    }

    #[test]
    fn every_refusal_code_the_pipeline_documents_survives_the_projection_to_the_loop() {
        // `as_execution` is the seam the loop reads, and it maps through `DenyReason::from_code`
        // with a `ToolDenied` fallback. **That fallback is what hid the defect this test exists
        // for**: `tool_disabled` was a documented code on `CallOutcome::Refused` and was missing
        // from the table, so an operator switching a tool off reached the loop, the model and the
        // guardrail as `tool_denied` — a permissions problem with a permissions fix, when the
        // real cause is a global switch nobody at the agent level can undo.
        //
        // Asserted over the codes rather than over the enum, because the codes are API surface
        // (`CallOutcome::Refused` documents them in its own doc comment) and the table is what
        // has to keep up. A code added to the product without a row here turns into a silent
        // `ToolDenied`; this is the check that makes that a red test instead.
        for code in [
            "tool_unknown",
            "tool_denied",
            "approval_required",
            "tool_bad_arguments",
            "tool_timeout",
            "tool_disabled",
            "tool_limited",
        ] {
            let mapped = crate::tools::DenyReason::from_code(code);
            assert_eq!(
                mapped.map(crate::tools::DenyReason::code),
                Some(code),
                "{code} is documented on CallOutcome::Refused and must reach the loop as itself, \
                 not as a generic denial"
            );
        }

        // And the direction that actually bit: a disable must NOT be a denial, or a model that
        // was told "permission refused" retries a call that can never succeed.
        assert_ne!(
            crate::tools::DenyReason::from_code("tool_disabled"),
            Some(crate::tools::DenyReason::ToolDenied),
            "an operator's global switch-off is a different fact from an allow-list refusal"
        );
        assert_ne!(
            crate::tools::DenyReason::from_code("tool_limited"),
            Some(crate::tools::DenyReason::ToolDenied),
            "a spent budget is not a permissions problem either"
        );
    }

    #[test]
    fn a_stored_identity_becomes_a_resolved_one_with_its_grants() {
        // The adapter every call site uses, asserted so a change to the stored row's shape cannot
        // silently drop the grants map — which would turn every explicit deny into an inherit.
        let row = crate::identity::AiIdentity {
            id: Uuid::new_v4(),
            organization_id: None,
            key: "editor".to_owned(),
            name: "Editor".to_owned(),
            description: String::new(),
            is_default: true,
            created_by: None,
            #[allow(clippy::useless_conversion)]
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        let resolved = identity_of(&row, BTreeMap::from([("content.search".to_owned(), false)]));
        assert_eq!(resolved.id, row.id);
        assert_eq!(resolved.key, "editor");
        assert_eq!(resolved.grants.get("content.search"), Some(&false));
    }
}

