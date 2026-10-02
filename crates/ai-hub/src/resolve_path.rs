//! The live call path: resolve against the maps, **record the decision**, hand back the pair.
//!
//! Slice 3 shipped the decision log — the table, the writer, the screen and the pruner. What it
//! deliberately did not do was *call* it: a log that only receives rows from a test is a log that
//! stays empty in production, and an empty log is indistinguishable from a broken one. This
//! module is the missing call site.
//!
//! Four properties matter here, and each is the reason the code looks the way it does.
//!
//! **The row is written before the provider is dialled.** A decision is a routing *decision*: it
//! is settled the moment a model is chosen, whether or not that model then answers. A request
//! that times out still made a routing choice, and that choice is exactly what an operator needs
//! to see afterwards. Writing the row after the answer would lose precisely the rows worth
//! keeping.
//!
//! **A walk that cannot answer still returns.** [`Resolved`] carries the model when one answered
//! and the reasons when none did, and the caller — the chat endpoint, the agent runtime — decides
//! what to do with a refusal. Returning an error instead would make "unresolved" and "the
//! database is unreachable" the same failure, and only one of them is the operator's job.
//!
//! **The chosen pair is read back through the router, not guessed.** The resolver speaks in
//! `provider/model` identifiers because that is what the walk is *for* — it is a human-facing
//! explanation. Turning one into the two ids a usage row and a cost row join on goes back
//! through [`crate::router::resolve`], so the id pair cannot come from a second, subtly
//! different lookup of the model table.
//!
//! **The fallback index is converted once.** The resolver counts positions from 1 (the primary
//! is 1) and the column counts from 0, so writing the position straight through badges every
//! primary as a fallback. [`crate::decision_store::answer_position`] is the only place that does
//! that subtraction.

use sqlx::PgPool;

use crate::decision_store::{DecisionContext, NewDecision, record};
use crate::error::Result;
use crate::routing::{ResolveRequest, Scope, decide};
use crate::routing_store::load_maps;
use crate::router::ResolvedModel;

/// What one resolved request needs the caller to act on.
///
/// A caller dials a provider when [`Resolved::model`] is `Some`; when it is `None` the decision
/// row holds the walk and the reasons, and [`Resolved::decision_id`] is the row that explains
/// them.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// The provider and model that answer, when one did.
    pub model: Option<ResolvedModel>,
    /// The row the decision was written to. Present even for a refusal — that is the point.
    pub decision_id: i64,
    /// The rule that produced the answer (`task_route`, `feature_override`, …, `unresolved`).
    ///
    /// An owned string rather than a `&'static str`: the resolver's rules are static, but a
    /// decision may be **downgraded** to `unresolved` when the model it names disappears
    /// between the walk and the read (see below), so this value is not always the constant the
    /// resolver handed over.
    pub rule: String,
    /// True when nothing in the maps could answer.
    pub unresolved: bool,
    /// The requirement set the decision was taken under, in the spelling the row stores. The
    /// caller needs it to answer the next request the same way.
    pub requirements: Vec<String>,
    /// Whether the walk had **any candidate to consider** when it failed.
    ///
    /// This is the difference between two failures that a caller must not be told about
    /// identically. A `false` here means the maps hold nothing at all for this request — the
    /// installation has no default model, the provider list is empty, nothing has ever been
    /// configured — which is a **setup** problem and a `409`. A `true` means a candidate existed
    /// and was refused for a stated reason (a missing capability, a pin naming a model that is
    /// switched off), which is a **routing** problem and a `422` with the walk to show.
    ///
    /// Collapsing the two into one status is the mistake this field exists to prevent: the first
    /// asks an operator to go and set a default model, the second asks them to look at a
    /// specific row. A caller told "unresolved" for an empty installation is sent to a log
    /// screen that is empty for the same reason the answer was.
    pub had_candidates: bool,
}

/// Resolve one request, record the decision, and return what answered.
///
/// `requested` is what the caller asked for — `provider/model`, a bare key, or nothing — and it
/// is stored on the row verbatim. Storing the *resolved* pair instead would make the log unable
/// to answer "what did the caller actually ask for", which is the first question a fallback
/// badge provokes.
///
/// An explicit model, when named, is resolved through the real router so the recorded walk is
/// the walk a live request takes: a name that does not exist is refused here, before a row is
/// written, rather than logged as a successful answer.
pub async fn resolve_and_record(
    pool: &PgPool,
    context: DecisionContext<'_>,
    scope: Scope,
    requested: Option<&str>,
) -> Result<Resolved> {
    let chain = scope.chain(context.organization_id);
    let maps = load_maps(pool, &chain).await?;

    let explicit = match requested.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => Some(crate::router::resolve(pool, Some(value)).await?.model),
        None => None,
    };

    let requirements = context.requirements.to_vec();
    let decision = decide(
        &maps,
        &ResolveRequest {
            explicit: explicit.as_ref(),
            // The caller's own spelling, so the walk records something `load_pair` can read
            // back to the *same* pair rather than to whatever a bare key happens to resolve to.
            explicit_identifier: requested.map(str::trim).filter(|v| !v.is_empty()),
            feature: context.feature,
            task: context.task,
            requires: requirements.clone(),
            scope,
            organization_id: context.organization_id,
        },
    );

    // The pair the caller dials, looked up **by the identifier the walk itself chose**.
    //
    // The first draft asked `router::resolve` for it, on the reasoning that one resolver is
    // better than two. It is worse, and provably: `router::resolve` answers for the
    // *installation's* view, while this walk answers for a scope chain that may be a site's or
    // an organization's. When the two disagree the row stored one rule and the ids of a
    // different model — which the table refuses outright
    // (`ai_route_decisions_answer_agrees_with_rule`: a non-`unresolved` rule must carry a
    // model), so every tenant-scoped chat became a 500.
    //
    // Reading the walk's own identifier back through the model table keeps the two facts
    // inseparable: the ids belong to the decision that produced them, because they come from it.
    let model = match decision.model.as_ref() {
        Some(candidate) => load_pair(pool, &candidate.model_id).await?,
        None => None,
    };

    // The rule and the answer must agree, and the table enforces it. A `None` model under any
    // rule other than `unresolved` would violate that, so the row is downgraded to
    // `unresolved` rather than written as a contradiction: the model was removed between the
    // walk and this read (a deployment change landing mid-request), which *is* an unresolved
    // request as far as the caller is concerned.
    let answer = match model.as_ref() {
        Some(resolved) if !decision.unresolved => Some(crate::decision_store::Answer {
            provider_id: resolved.provider.id,
            model_id: resolved.model.id,
            // The 1-based → 0-based conversion, in the one function that owns it.
            fallback_index: crate::decision_store::answer_position(decision.model.clone()),
        }),
        _ => None,
    };
    let unresolved = decision.unresolved || answer.is_none();

    let new = NewDecision::from_decision(&decision, context, answer);
    // The rule travels with the row, so a downgraded decision says `unresolved` rather than a
    // rule whose own precondition the row does not meet.
    let mut new = new;
    if unresolved && new.rule != "unresolved" {
        new.rule = "unresolved".to_owned();
    }
    let decision_id = record(pool, &new).await?;

    // Whether the maps held **any row** for this request — a task route, a feature pin, or an
    // installation default.
    //
    // Not "the walk names a model", which was the first attempt and is wrong in the case that
    // matters: the maps filter out a disabled model *before* the walk sees it, so a request
    // whose only default was switched off produces a walk with nothing in it and would be
    // reported as "nothing was ever configured" — sending the operator to set a default model
    // they had already set, and had already switched off on purpose.
    //
    // The three questions are genuinely different, and the caller answers a different one for
    // each: "you have configured nothing" (set a default), "the row you configured cannot
    // answer" (fix that row), "the model is switched off" (switch it back on). Collapsing them
    // into one status is the mistake this field exists to prevent.
    // Deliberately a question about the **database**, not about the filtered maps. Every other
    // signal here is the same problem: a model that was deliberately switched off is already
    // gone from `maps`, so an installation whose only model an operator turned off looks exactly
    // like one that was never set up. They are not the same problem and the caller answers them
    // differently — one is "switch it back on", the other is "go and set a default". Any row at
    // all is the honest threshold. See `store::any_model_registered`.
    let had_candidates = crate::store::any_model_registered(pool).await?;

    Ok(Resolved {
        model: if unresolved { None } else { model },
        decision_id,
        rule: new.rule.clone(),
        unresolved,
        requirements,
        had_candidates,
    })
}

/// The stored row for one `provider/model` identifier, or `None` when the name no longer exists.
///
/// The split is on the **first** `/`, and that only works because the prefix is a provider name
/// this platform assigned. A model key may itself contain a slash — a llama.cpp endpoint
/// publishes `models/<file>.gguf`, Ollama publishes namespaced ids — so a bare key of
/// `models/x` must not be read as "provider `models`, model `x`". The prefix is therefore
/// resolved first and the whole string is tried as a key when no provider carries that name.
///
/// A name whose provider is gone, or whose model is not on that provider, resolves to `None` —
/// and the caller downgrades the decision, which is the honest answer.
async fn load_pair(pool: &PgPool, identifier: &str) -> Result<Option<ResolvedModel>> {
    // Resolve the provider prefix **first**, and commit to it. The tempting version — "try the
    // prefix, and if that does not work try the whole string as a key" — is wrong in the one
    // case that matters: a prefixed name whose model is disabled would fall through to the bare
    // lookup, find *another* provider serving the same key, and answer with it. A request that
    // said `Standby/mock-small` would be served by `Preferred/mock-small`, which is not a
    // fallback (an explicit pin has none) but a substitution, and the operator's pin silently
    // addressed a different machine.
    if let Some((provider_name, rest)) = identifier.split_once('/')
        && let Some(provider) = crate::store::find_provider_by_name(pool, provider_name).await?
    {
        // The provider is named, so the model is looked up on **that** provider and nowhere
        // else. `None` here means "this provider does not serve an enabled model by that name",
        // which is an answer, not a reason to keep looking.
        let model = crate::store::find_model_by_key(pool, provider.id, rest)
            .await?
            .filter(|model| model.enabled);

        return Ok(model.map(|model| ResolvedModel { provider, model }));
    }

    // No prefix, or the prefix is not a provider here — both are the same question, and
    // `router::resolve` answers it with the same lookup the request itself would have used. This
    // is also the branch that makes a model key *containing* a slash reachable: a llama.cpp
    // endpoint publishes `models/<file>.gguf`, so `models/x` names a model, not a provider
    // called `models`. Without it, every pinned request to a local runtime came back `None` and
    // was downgraded to `unresolved` with an empty walk to explain it.
    Ok(crate::router::resolve(pool, Some(identifier)).await.ok())
}
