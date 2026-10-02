//! The model catalog's own vocabulary (docs/requests/REQ-098, slice 1).
//!
//! REQ-097 gave every model row *flags* — `supports_tools`, `supports_vision` and the rest — and
//! the router reads them. This module adds what turns a flag list into a **catalog an operator
//! can act on**: what a model costs, how the cost is dated and sourced, and the one rule that
//! makes a cost list honest rather than decorative.
//!
//! # The rule the rest of the request hangs on
//!
//! **A price edit changes the cost of the next request only.** The engine records the money a
//! call actually cost at the moment it ran, in the same row that holds its token counts. Nothing
//! in the platform recomputes historical cost from a current price, because a bill that changes
//! after the fact is worse than one that was approximately right all along. That is why
//! [`ModelPrice`] is a value with a [`PriceSource`](self::PriceSource) and a date, and why
//! [`validate_price`] refuses a negative number rather than letting a credit slip through as a
//! "discount" nobody chose.
//!
//! The unit throughout is **micros of a currency per million tokens** (`MICROS_PER_MTOK`). The
//! accounting the engine owns sums in micros so a whole-cent price never rounds to nothing, and
//! a vendor's price page quotes a per-million figure; the panel may render per 1K for reading,
//! but the column stays per million because that is the number being compared to the vendor.

use serde::{Deserialize, Serialize};
use time::{Date, OffsetDateTime};
use uuid::Uuid;

use crate::error::{AiHubError, Result};
use crate::model::{AiModel, ModelCapability};

/// One million — the denominator of every price column and every derived figure.
pub const TOKENS_PER_MTOK: i64 = 1_000_000;

/// The smallest price a per-million column can express: one micro per million tokens.
///
/// A vendor that charges less than that is, for accounting purposes, free — the number rounds
/// to zero in any currency an invoice is printed in. The floor exists so `format_micros` has a
/// defined answer for a non-zero price and never prints "0" for money that was actually spent.
pub const MICROS_FLOOR_PER_MTOK: i64 = 1;

/// Largest price a per-million column accepts.
///
/// A megabyte-scale price is a typo — 10^15 micros per million tokens is a billion currency
/// units per million tokens, and no vendor is anywhere near it. The ceiling turns a mistyped
/// 16 into a refusal the operator can read rather than a cost report with six extra digits.
pub const MICROS_CEILING_PER_MTOK: i64 = 1_000_000_000;

/// The seven routing tasks, as a closed vocabulary.
///
/// A closed set on purpose: the routing screen renders one row per task, the dry-run endpoint
/// refuses a task it does not know, and neither has to handle a task key nobody chose. The
/// descriptions are what the panel prints under each name, so a task that could be mistaken for
/// a neighbour says what it is for.
pub const ROUTING_TASKS: &[&str] = &[
    "cheap",
    "translation",
    "coding",
    "vision",
    "long_context",
    "embedding",
    "critical",
];

/// What each routing task is for, in the panel's own words.
#[must_use]
pub fn task_description(task: &str) -> &'static str {
    match task {
        "cheap" => "Short, high-volume work where a small model's quality is enough.",
        "translation" => "Language conversion, where fluency beats reasoning depth.",
        "coding" => "Code generation and review, which needs a strong tool-calling model.",
        "vision" => "Work over images, which needs a model that can see.",
        "long_context" => "Documents too large for a short context window.",
        "embedding" => "Turning text into vectors for retrieval; picks the embedding model.",
        "critical" => "The work that must be right; the first candidate is the strongest one.",
        _ => "A routing task the platform does not recognise.",
    }
}

/// A task key the platform accepts, or a refusal that names the whole vocabulary.
///
/// The refusal lists every task rather than saying "invalid task", because the caller is
/// almost always a person who guessed at the key and every option is a legitimate choice.
pub fn validate_task(task: &str) -> Result<&'static str> {
    ROUTING_TASKS
        .iter()
        .copied()
        .find(|known| *known == task)
        .ok_or_else(|| {
            AiHubError::InvalidModel(format!(
                "task \"{task}\" is not one of the routing tasks ({})",
                ROUTING_TASKS.join(", ")
            ))
        })
}

/// The named features a caller may pin its own model for.
///
/// A feature key is an **interface**, not a label: adding one here means the override form, the
/// dry-run resolver and this list all change together, because a feature pin that silently does
/// nothing is worse than no override at all. Every entry therefore says what a caller is asking
/// for when it uses it.
pub const MODEL_FEATURES: &[&str] = &[
    "content_assist",
    "copilot",
    "chat",
    "translate",
    "seo",
    "alt_text",
    "summarize",
    "agent_default",
];

/// What a feature pin is for, in the panel's own words.
#[must_use]
pub fn feature_description(feature: &str) -> &'static str {
    match feature {
        "content_assist" => "Writing and editing help inside the CMS editor.",
        "copilot" => "The in-module copilot that answers a question about the current record.",
        "chat" => "The platform's own chat surface.",
        "translate" => "Translating content between locales.",
        "seo" => "Metadata and description drafting.",
        "alt_text" => "Describing an image for its alternative text.",
        "summarize" => "Condensing a long document.",
        "agent_default" => "The model an agent falls back to when it names none.",
        _ => "A feature the platform does not recognise.",
    }
}

/// One known feature, as the override form reads it.
///
/// A feature key is an interface, and this is the one place the interface is described — the
/// override form, the dry-run resolver and this struct all read the same text, so adding a
/// feature in one place cannot leave the other two describing something different.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelFeatures {
    /// The wire key.
    pub key: String,
    /// What a caller is asking for when it pins a model to this feature.
    pub description: String,
}

/// Every known feature, in the order the override form lists them.
///
/// Returned rather than re-derived at each call site: the order is part of the form's contract
/// (a select whose options reshuffle between two loads is unusable) and the descriptions travel
/// with the keys so the client never has to hard-code prose the crate already owns.
#[must_use]
pub fn model_features() -> Vec<ModelFeatures> {
    MODEL_FEATURES
        .iter()
        .map(|key| ModelFeatures {
            key: (*key).to_owned(),
            description: feature_description(key).to_owned(),
        })
        .collect()
}

/// Every routing task, in the order the routing screen lists them.
#[must_use]
pub fn routing_tasks() -> Vec<ModelFeatures> {
    ROUTING_TASKS
        .iter()
        .map(|key| ModelFeatures {
            key: (*key).to_owned(),
            description: task_description(key).to_owned(),
        })
        .collect()
}

/// Check a feature key the platform accepts, with the same listing refusal as [`validate_task`].
pub fn validate_feature(feature: &str) -> Result<&'static str> {
    MODEL_FEATURES
        .iter()
        .copied()
        .find(|known| *known == feature)
        .ok_or_else(|| {
            AiHubError::InvalidModel(format!(
                "feature \"{feature}\" is not a known feature ({})",
                MODEL_FEATURES.join(", ")
            ))
        })
}

/// The four requirements a task route may pin on a candidate list.
///
/// Four and not more: the routing screen has room for four chips in a cell before the row wraps
/// on a laptop, and every requirement added is one more way a route becomes unreadable. A
/// requirement is a *capability* the candidate must claim, not a preference.
pub const ROUTE_REQUIREMENTS: &[&str] = &["tools", "vision", "long_context", "json"];

/// Check a requirement key against [`ROUTE_REQUIREMENTS`].
pub fn validate_requirement(requirement: &str) -> Result<&'static str> {
    ROUTE_REQUIREMENTS
        .iter()
        .copied()
        .find(|known| *known == requirement)
        .ok_or_else(|| {
            AiHubError::InvalidModel(format!(
                "requirement \"{requirement}\" is not one of the route requirements ({})",
                ROUTE_REQUIREMENTS.join(", ")
            ))
        })
}

/// Which capability a route requirement asks a candidate to claim.
///
/// `long_context` is not a stored flag: it is a *fact about the context window*, and a route
/// asking for it against a model whose window is unknown is answered by refusing rather than by
/// guessing. That asymmetry is deliberate — a requirement the platform can only guess at is a
/// requirement it will eventually get wrong in production.
#[must_use]
pub fn requirement_capability(requirement: &str) -> Option<ModelCapability> {
    match requirement {
        "tools" => Some(ModelCapability::Tools),
        "vision" => Some(ModelCapability::Vision),
        "json" => Some(ModelCapability::JsonMode),
        "long_context" | "embedding" => None,
        _ => None,
    }
}

/// How much context a model must have to count as "long context".
///
/// 32k tokens. It is a line somebody has to draw, and drawing it in one named constant means the
/// answer to "why did this candidate get skipped" is a sentence an operator can check rather than
/// a number that changed last month.
pub const LONG_CONTEXT_TOKENS: i32 = 32_000;

/// The smallest context window a `long_context` requirement accepts.
pub const fn long_context_minimum() -> i32 {
    LONG_CONTEXT_TOKENS
}

/// Where a stored price came from.
///
/// The three values are the whole story of how much an operator should trust the number, so the
/// model screen shows it rather than hiding it: a hand-typed price is an estimate, a price an
/// endpoint reported is that endpoint's own claim, and a price a live probe measured is a
/// measurement. A source nobody recognises would render as a blank badge, so the set is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceSource {
    /// An operator typed it.
    Manual,
    /// The provider's own listing reported it.
    Discovery,
    /// A live probe measured it.
    Probe,
}

impl PriceSource {
    /// Every source, in the order the panel lists them.
    pub const ALL: &'static [Self] = &[Self::Manual, Self::Discovery, Self::Probe];

    /// Wire name, as stored and as the panel's badge reads it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Discovery => "discovery",
            Self::Probe => "probe",
        }
    }

    /// What the panel says about a price with this source, so the estimate framing is not
    /// re-typed in four places and drift away from the vocabulary.
    #[must_use]
    pub fn note(self) -> &'static str {
        match self {
            Self::Manual => "Entered by an operator. Treat it as an estimate.",
            Self::Discovery => "Reported by the provider's own listing.",
            Self::Probe => "Measured by a live probe.",
        }
    }

    /// Read a wire name.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|source| source.as_str() == value)
    }
}

impl Default for PriceSource {
    /// `Manual` is the default because it is the *least* claim the platform can make about a
    /// number: it asserts only that a person wrote it down. Every other source asserts the
    /// platform verified it, so defaulting to one of those would be a claim the code has not
    /// earned. The stored column is `not null default 'manual'` for the same reason.
    fn default() -> Self {
        Self::Manual
    }
}

impl std::fmt::Display for PriceSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What one model costs, and how sure the platform is.
///
/// Both halves are optional and the difference is not cosmetic: a model whose **input** price is
/// known and whose **output** price is not is genuinely half-priced, and the panel says so
/// rather than rendering a zero that would make it look free.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPrice {
    /// Micros per million input tokens, when known.
    pub input_micros_per_mtok: Option<i64>,
    /// Micros per million output tokens, when known.
    pub output_micros_per_mtok: Option<i64>,
    /// Where the number came from.
    pub source: PriceSource,
    /// When it was written down.
    pub updated_at: Option<OffsetDateTime>,
}

impl ModelPrice {
    /// A price nobody has written yet: the state a freshly discovered model is in.
    ///
    /// `Manual` rather than a fourth "unknown" source, because the *source* of an absent price is
    /// genuinely meaningless — what matters is that the number is absent, and `updated_at: None`
    /// already says that.
    #[must_use]
    pub fn unknown() -> Self {
        Self {
            source: PriceSource::Manual,
            ..Self::default()
        }
    }

    /// `true` when at least one of the two halves is known.
    #[must_use]
    pub fn is_known(&self) -> bool {
        self.input_micros_per_mtok.is_some() || self.output_micros_per_mtok.is_some()
    }

    /// `true` when **both** halves are known — the only state a cost estimate can be built from
    /// without inventing the missing half.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.input_micros_per_mtok.is_some() && self.output_micros_per_mtok.is_some()
    }

    /// What a call of this size costs, in micros, or `None` when the price cannot answer.
    ///
    /// `None` is a real answer and the caller must handle it: a partially-priced model has an
    /// unknown cost, not a cheap one. Returning a number built from the half that happens to be
    /// known would understate every call and is the kind of quiet wrongness an accounting screen
    /// must never ship.
    #[must_use]
    pub fn cost_micros(&self, prompt_tokens: i64, completion_tokens: i64) -> Option<i64> {
        // A negative count is a caller bug; clamping to zero keeps the arithmetic from silently
        // producing a credit.
        let prompt = prompt_tokens.max(0);
        let completion = completion_tokens.max(0);

        let input = self
            .input_micros_per_mtok
            .map(|rate| rate.saturating_mul(prompt) / TOKENS_PER_MTOK);
        let output = self
            .output_micros_per_mtok
            .map(|rate| rate.saturating_mul(completion) / TOKENS_PER_MTOK);

        match (input, output) {
            (Some(input), Some(output)) => Some(input.saturating_add(output)),
            // Either half unknown means the total is unknown. Returning the known half would
            // understate every call by the missing rate, which is the quiet wrongness this whole
            // module exists to prevent.
            _ => None,
        }
    }

    /// The panel's per-1K rendering of one half, in the currency's minor unit.
    ///
    /// Per 1K is what a person reads; per million is what the column stores. The division
    /// rounds **up** to the nearest micro so a rate below one micro per 1K still prints as
    /// something — rounding a real cost down to nothing is the failure mode the floor exists to
    /// prevent.
    #[must_use]
    pub fn per_1k_micros(&self, half: PriceHalf) -> Option<i64> {
        let rate = match half {
            PriceHalf::Input => self.input_micros_per_mtok,
            PriceHalf::Output => self.output_micros_per_mtok,
        }?;
        if rate == 0 {
            return Some(0);
        }

        // ceil(rate / 1000) without floating point, and without an overflow on a rate at the
        // documented ceiling. `div_ceil` is still unstable in this toolchain, so the ceiling is
        // written out: integer division plus one, except when it already divides evenly.
        Some((rate + 999) / 1_000)
    }
}

/// Which half of a price a caller is asking about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceHalf {
    /// What the model is fed.
    Input,
    /// What the model writes back.
    Output,
}

/// Check a price before it is stored, and say which half refused it.
///
/// The two halves are checked independently and both problems are named in one message: an
/// operator who typed a bad output price on a row they also mistyped the input price on should
/// not have to save, fail, fix, save, fail again.
pub fn validate_price(input: Option<i64>, output: Option<i64>) -> Result<()> {
    let mut refused: Vec<&str> = Vec::new();
    for (half, value) in [("input", input), ("output", output)] {
        match value {
            None => {}
            Some(0) => refused.push(half),
            Some(amount) if amount < 0 => refused.push(half),
            Some(amount) if amount > MICROS_CEILING_PER_MTOK => refused.push(half),
            Some(_) => {}
        }
    }

    if refused.is_empty() {
        return Ok(());
    }

    Err(AiHubError::InvalidModel(format!(
        "{} price must be null or between 1 and {MICROS_CEILING_PER_MTOK} micros per million tokens",
        refused.join(" and ")
    )))
}

/// How a model filters against a capability selection.
///
/// The catalog's filter is a **conjunction**: a row shows when it claims *every* capability the
/// operator selected. This is the only reading that is useful for a capability filter — a
/// disjunction would show a vision model for a query of "tools OR vision" and then hide the fact
/// that it cannot do the other half, which is exactly the confusion the filter exists to remove.
#[derive(Debug, Clone, Default)]
pub struct CapabilityFilter {
    /// Every one of these must be true for a row to pass.
    pub required: Vec<ModelCapability>,
}

impl CapabilityFilter {
    /// A filter over one selection.
    #[must_use]
    pub fn new(required: Vec<ModelCapability>) -> Self {
        Self { required }
    }

    /// `true` when the model passes. An empty filter passes everything, so a cleared chip row is
    /// "no filter" rather than "no models".
    #[must_use]
    pub fn matches(&self, model: &AiModel) -> bool {
        self.required
            .iter()
            .all(|capability| model.capability(*capability))
    }

    /// Keep only the models that pass.
    #[must_use]
    pub fn apply<'a>(&self, models: &'a [AiModel]) -> Vec<&'a AiModel> {
        models.iter().filter(|model| self.matches(model)).collect()
    }
}

/// What the catalog says about one model's **use**: the routes, overrides and agents pointing at
/// it.
///
/// The count is what the panel's "Used by" column prints, but the *reason* matters more: a model
/// with no inbound route is a candidate nothing will ever ask for, and an operator switching one
/// off needs to know that before they do. So the catalog answers with named sources, and
/// [`ModelUsage::is_unreferenced`] is what the "nobody routes here" note keys off.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelUsage {
    /// Task routes naming this model, as `task` keys.
    pub tasks: Vec<String>,
    /// Feature overrides naming this model, as feature keys.
    pub features: Vec<String>,
    /// `true` when the model is the installation default.
    pub is_default: bool,
}

impl ModelUsage {
    /// `true` when nothing in the installation points at this model.
    #[must_use]
    pub fn is_unreferenced(&self) -> bool {
        self.tasks.is_empty() && self.features.is_empty() && !self.is_default
    }

    /// How many places name this model, for the column's count.
    #[must_use]
    pub fn reference_count(&self) -> usize {
        self.tasks.len() + self.features.len() + usize::from(self.is_default)
    }
}

/// One price row as the panel reads it, with the derived figures already computed.
///
/// The API returns this rather than a bare `ModelPrice` because the two per-1K figures and the
/// per-million figures are the *same* number at two precisions, and computing them in Rust means
/// the table, the detail screen and the CSV export cannot each round differently.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceView {
    /// Micros per million input tokens.
    pub input_micros_per_mtok: Option<i64>,
    /// Micros per million output tokens.
    pub output_micros_per_mtok: Option<i64>,
    /// Per-1K rendering of the input half.
    pub input_micros_per_1k: Option<i64>,
    /// Per-1K rendering of the output half.
    pub output_micros_per_1k: Option<i64>,
    /// Where the number came from.
    pub source: PriceSource,
    /// What the panel says about that source.
    pub source_note: String,
    /// When it was written down.
    pub updated_at: Option<OffsetDateTime>,
    /// `true` when both halves are known.
    pub complete: bool,
}

impl From<&ModelPrice> for PriceView {
    fn from(price: &ModelPrice) -> Self {
        Self {
            input_micros_per_mtok: price.input_micros_per_mtok,
            output_micros_per_mtok: price.output_micros_per_mtok,
            input_micros_per_1k: price.per_1k_micros(PriceHalf::Input),
            output_micros_per_1k: price.per_1k_micros(PriceHalf::Output),
            source: price.source,
            source_note: price.source.note().to_owned(),
            updated_at: price.updated_at,
            complete: price.is_complete(),
        }
    }
}

/// One catalog row: the model, its provider, its price and its usage.
///
/// A single row the table renders and the detail screen reads, so "what the catalog says" is one
/// value rather than four parallel arrays the client has to join — and a join the client does
/// with a `find` is a row that silently disappears when the id does not match.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogEntry {
    /// Model id.
    pub id: Uuid,
    /// Provider id.
    pub provider_id: Uuid,
    /// Provider display name.
    pub provider_name: String,
    /// Wire key.
    pub model_key: String,
    /// Panel label.
    pub display_name: String,
    /// The `provider/model` identifier the router and the logs speak.
    pub pair_id: String,
    /// Context window in tokens.
    pub context_window: Option<i32>,
    /// Largest answer.
    pub max_output_tokens: Option<i32>,
    /// The capability flags that are on.
    pub capabilities: Vec<String>,
    /// The price, with its derived per-1K figures.
    pub price: PriceView,
    /// What points at this model.
    pub usage: ModelUsage,
    /// Whether the model is enabled.
    pub enabled: bool,
    /// Whether it is the installation default.
    pub is_default: bool,
    /// Where the capability flags came from.
    pub capabilities_source: String,
    /// When the flags were last confirmed.
    pub capabilities_verified_at: Option<OffsetDateTime>,
    /// When the model was registered.
    pub created_at: OffsetDateTime,
    /// When it was last changed.
    pub updated_at: OffsetDateTime,
}

/// How stale a price may get before the model screen says so.
///
/// Ninety days. Long enough that nobody is nagged for a price they entered once, short enough
/// that a price nobody revisited is not still presented as current a year later. The screen shows
/// the age rather than a badge, because "entered 4 months ago" says more than a red dot.
pub const PRICE_STALE_DAYS: i64 = 90;

/// How stale a price is, in whole days, as of `now`.
///
/// `None` when the price was never written — which is not the same as "fresh", and the caller
/// renders it as "no price" rather than as zero days old.
#[must_use]
pub fn price_age_days(updated_at: Option<OffsetDateTime>, now: OffsetDateTime) -> Option<i64> {
    let updated = updated_at?;
    let days = (now - updated).whole_days();
    Some(days.max(0))
}

/// `true` when a price is old enough for the screen to flag it.
#[must_use]
pub fn price_is_stale(updated_at: Option<OffsetDateTime>, now: OffsetDateTime) -> bool {
    price_age_days(updated_at, now).is_some_and(|days| days > PRICE_STALE_DAYS)
}

/// Order two optional prices so that **a price nobody wrote sorts last**.
///
/// The derived `Ord` on `Option` is the trap this function exists to defuse: `None` compares
/// **less than** `Some(_)`, so `a.cmp(&b)` on two raw price columns puts every unpriced model at
/// the *top* of a column headed "cheapest first" — the exact inversion the catalog has to avoid,
/// and the one an operator reads as "these models are free". The SQL fragment in
/// [`CatalogSort::order_by`] spells the rule out as `nulls last` because Postgres does not share
/// Rust's ordering, so the rule had to be written twice; this is the one place it is *derived*,
/// and the API's in-memory sort calls it rather than comparing the raw columns.
///
/// Two unpriced models tie here, and the caller breaks the tie by model key — an unpriced row
/// has no cost to distinguish it from another unpriced row, so the only honest order left is
/// alphabetical.
#[must_use]
pub fn cmp_price(left: Option<i64>, right: Option<i64>) -> std::cmp::Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.cmp(&right),
        // Known before unknown, in both directions, so a swap cannot flip the order.
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

/// Whether a model can serve the `embedding` task — the one capability rule REQ-098's acceptance
/// criteria call out by name.
///
/// It is a function rather than an inline check because the same rule appears in three places
/// (the route validator, the router's skip reason and the panel's select filter) and three copies
/// of a rule is two copies too many: an embedding-only model must never be pickable as a
/// collection's embedding model, and a *chat* model must not quietly satisfy the task.
#[must_use]
pub fn can_serve_task(task: &str, model: &AiModel) -> bool {
    match task {
        "embedding" => model.capability(ModelCapability::Embeddings),
        "vision" => model.capability(ModelCapability::Vision),
        "coding" | "critical" => model.capability(ModelCapability::Tools),
        "long_context" => model
            .context_window
            .is_some_and(|window| window >= LONG_CONTEXT_TOKENS),
        // Translation, cheap and the rest have no structural requirement: they are preferences
        // expressed by the order of the candidate list, not flags a model must claim.
        _ => true,
    }
}

/// Why a model cannot serve a task, in a sentence an operator can act on.
///
/// `None` when it can. Every branch names the *requirement* that refused it, because "incompatible
/// model" is the message that gets a support ticket and "does not claim the embeddings flag" is
/// the message that gets fixed in ten seconds.
#[must_use]
pub fn task_refusal_reason(task: &str, model: &AiModel) -> Option<String> {
    if can_serve_task(task, model) {
        return None;
    }

    Some(match task {
        "embedding" => format!(
            "\"{}\" does not claim the embeddings flag, so it cannot serve an embedding collection",
            model.model_key
        ),
        "vision" => format!(
            "\"{}\" does not claim the vision flag, so it cannot serve the vision task",
            model.model_key
        ),
        "coding" | "critical" => format!(
            "\"{}\" does not claim the tools flag, so it cannot serve the {task} task",
            model.model_key
        ),
        "long_context" => match model.context_window {
            Some(window) => format!(
                "\"{}\" has a context window of {window} tokens, below the {LONG_CONTEXT_TOKENS} the long-context task asks for",
                model.model_key
            ),
            None => format!(
                "\"{}\" has no context window recorded, so it cannot be asked for a long-context task",
                model.model_key
            ),
        },
        _ => format!("\"{}\" cannot serve the {task} task", model.model_key),
    })
}

/// Whether a model satisfies one route requirement, and the reason when it does not.
#[must_use]
pub fn requirement_refusal_reason(requirement: &str, model: &AiModel) -> Option<String> {
    if let Some(capability) = requirement_capability(requirement) {
        if !model.capability(capability) {
            return Some(format!(
                "\"{}\" does not claim the {requirement} capability",
                model.model_key
            ));
        }
        return None;
    }

    if requirement == "long_context" {
        return match model.context_window {
            Some(window) if window >= LONG_CONTEXT_TOKENS => None,
            Some(window) => Some(format!(
                "\"{}\" has a context window of {window} tokens, below the {LONG_CONTEXT_TOKENS} the requirement asks for",
                model.model_key
            )),
            None => Some(format!(
                "\"{}\" has no context window recorded, so the long_context requirement cannot be satisfied",
                model.model_key
            )),
        };
    }

    Some(format!(
        "\"{}\" cannot be checked against the requirement \"{requirement}\"",
        model.model_key
    ))
}

/// Today, as a `Date`, for the price-age arithmetic the panel renders.
#[must_use]
pub fn today(now: OffsetDateTime) -> Date {
    now.date()
}

// ---------------------------------------------------------------------------------------------
// The catalog query
// ---------------------------------------------------------------------------------------------

/// Which catalog column the table is sorted by.
///
/// A closed set because a sort key reaches SQL as an identifier: a caller-supplied string there
/// is either an injection or a whitelist check, and the whitelist *is* the type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogSort {
    /// `provider/model`, which is what the table's first column shows.
    #[default]
    Model,
    /// Provider name.
    Provider,
    /// Context window, largest first.
    Context,
    /// Input price, cheapest first — the sort a cost-conscious operator reaches for.
    Price,
    /// When the model was last changed.
    Updated,
}

impl CatalogSort {
    /// Every sort key, in the order the column headers list them.
    pub const ALL: &'static [Self] = &[
        Self::Model,
        Self::Provider,
        Self::Context,
        Self::Price,
        Self::Updated,
    ];

    /// Wire name, as the query string carries it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Provider => "provider",
            Self::Context => "context",
            Self::Price => "price",
            Self::Updated => "updated",
        }
    }

    /// Read a wire name, falling back to the default rather than failing.
    ///
    /// A sort key is a display preference, not a request: an unrecognised one shows the default
    /// order. Refusing it would make a stale bookmark render an error page over a table that
    /// works perfectly well.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        Self::ALL
            .iter()
            .copied()
            .find(|sort| sort.as_str() == value)
            .unwrap_or_default()
    }

    /// The SQL `ORDER BY` fragment this key stands for.
    ///
    /// A closed mapping rather than a formatted string: the key is a `&'static str` chosen by a
    /// `match`, so there is no path by which a caller's text reaches the statement.
    #[must_use]
    pub fn order_by(self) -> &'static str {
        match self {
            Self::Model => "m.model_key asc, p.name asc",
            Self::Provider => "p.name asc, m.model_key asc",
            // `nulls last` on both: a model whose window nobody recorded is not "the smallest",
            // and sorting it above a 4K model would put the least-known row at the top of a
            // column an operator reads to find the model that can hold their document.
            Self::Context => "m.context_window desc nulls last, m.model_key asc",
            Self::Price => {
                "m.input_cost_micros_per_mtok asc nulls last, m.output_cost_micros_per_mtok asc nulls last, m.model_key asc"
            }
            Self::Updated => "m.updated_at desc, m.model_key asc",
        }
    }
}

/// How a catalog listing is narrowed.
#[derive(Debug, Clone, Default)]
pub struct CatalogQuery {
    /// Free text, matched against the model key, the display name and the provider name.
    pub q: Option<String>,
    /// Capabilities a row must **all** claim.
    pub capabilities: Vec<ModelCapability>,
    /// One provider's id.
    pub provider_id: Option<Uuid>,
    /// `enabled` or `disabled`; `None` is both.
    pub status: Option<bool>,
    /// Which column the table is sorted by.
    pub sort: CatalogSort,
}

impl CatalogQuery {
    /// Narrow to one provider.
    #[must_use]
    pub fn for_provider(mut self, provider_id: Uuid) -> Self {
        self.provider_id = Some(provider_id);
        self
    }

    /// Require a set of capabilities, all of them.
    #[must_use]
    pub fn requiring(mut self, capabilities: Vec<ModelCapability>) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Read the query's `capability` parameter, which arrives comma-separated.
    ///
    /// An unknown key in the list is a refusal, not a silent drop: a caller asking for
    /// `tools,telepathy` and getting a table filtered by `tools` alone would believe they had
    /// narrowed it by two capabilities and had not.
    pub fn capabilities_from_param(value: Option<&str>) -> Result<Vec<ModelCapability>> {
        let Some(raw) = value else {
            return Ok(Vec::new());
        };

        raw.split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(ModelCapability::parse)
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                AiHubError::InvalidModel(format!(
                    "capability filter accepts only the model's own flags ({})",
                    ModelCapability::ALL
                        .iter()
                        .map(|capability| capability.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
    }

    /// Whether a model passes the whole narrowing, as one value.
    ///
    /// The search is a case-insensitive substring over three fields and nothing else. It is
    /// applied here rather than in SQL because the capability filter already needs the row in
    /// memory, and doing the text match in Rust keeps "what does search match" answerable in one
    /// place instead of splitting across a `where` clause and a `Vec::retain`.
    #[must_use]
    pub fn matches(&self, model: &AiModel, provider_name: &str) -> bool {
        if let Some(provider_id) = self.provider_id
            && model.provider_id != provider_id
        {
            return false;
        }
        if let Some(enabled) = self.status
            && model.enabled != enabled
        {
            return false;
        }

        let filter = CapabilityFilter::new(self.capabilities.clone());
        if !filter.matches(model) {
            return false;
        }

        match self.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
            None => true,
            Some(needle) => {
                let needle = needle.to_lowercase();
                model.model_key.to_lowercase().contains(&needle)
                    || model.label().to_lowercase().contains(&needle)
                    || provider_name.to_lowercase().contains(&needle)
            }
        }
    }
}

/// Turn a stored model plus its provider into the row the panel renders.
///
/// The one place a `CatalogEntry` is built, so the `pair_id` a row shows in the table, the
/// `pair_id` the copy button copies and the `pair_id` the detail screen opens are the same
/// string by construction rather than by three format calls agreeing today.
#[must_use]
pub fn catalog_entry(model: &AiModel, provider_name: &str) -> CatalogEntry {
    CatalogEntry {
        id: model.id,
        provider_id: model.provider_id,
        provider_name: provider_name.to_owned(),
        model_key: model.model_key.clone(),
        display_name: model.label().to_owned(),
        pair_id: format!("{provider_name}/{}", model.model_key),
        context_window: model.context_window,
        max_output_tokens: model.max_output_tokens,
        capabilities: model
            .capabilities()
            .into_iter()
            .map(|capability| capability.as_str().to_owned())
            .collect(),
        price: PriceView::from(&model.price()),
        usage: ModelUsage {
            tasks: Vec::new(),
            features: Vec::new(),
            is_default: model.is_default,
        },
        enabled: model.enabled,
        is_default: model.is_default,
        capabilities_source: model.capabilities_source.clone(),
        capabilities_verified_at: model.capabilities_verified_at,
        created_at: model.created_at,
        updated_at: model.updated_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn model(key: &str) -> AiModel {
        AiModel {
            id: Uuid::new_v4(),
            provider_id: Uuid::new_v4(),
            model_key: key.to_owned(),
            display_name: None,
            context_window: None,
            supports_tools: false,
            supports_vision: false,
            supports_streaming: true,
            supports_embeddings: false,
            supports_image_generation: false,
            supports_audio_generation: false,
            supports_transcription: false,
            supports_json_mode: false,
            max_output_tokens: None,
            input_cost_micros_per_mtok: None,
            output_cost_micros_per_mtok: None,
            price_source: "manual".to_owned(),
            price_updated_at: None,
            capabilities_source: "manual".to_owned(),
            capabilities_verified_at: None,
            enabled: true,
            is_default: false,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    // ---------------------------------------------------------------------------------------
    // The vocabularies
    // ---------------------------------------------------------------------------------------

    #[test]
    fn the_task_vocabulary_is_seven_and_closed() {
        assert_eq!(ROUTING_TASKS.len(), 7);
        for task in ROUTING_TASKS {
            assert_eq!(validate_task(task).expect("known"), *task);
            assert_ne!(task_description(task), task_description("nope"));
        }
        let refused = validate_task("cheapness").expect_err("unknown task");
        assert!(refused.to_string().contains("cheap"), "{refused}");
    }

    #[test]
    fn the_feature_vocabulary_is_closed_and_every_key_explains_itself() {
        for feature in MODEL_FEATURES {
            assert_eq!(validate_feature(*feature).expect("known"), *feature);
            // A feature whose description is the placeholder is a feature the override form
            // would render as a blank row.
            assert_ne!(feature_description(feature), feature_description("nope"));
        }
        assert!(validate_feature("chatbot").is_err());
    }

    /// The rule that two price columns must never be compared directly.
    ///
    /// `Option`'s derived ordering ranks `None` **first**, so the obvious sort prints every
    /// unpriced model at the top of a column headed "cheapest first". The test asserts the
    /// inverted behaviour on purpose as well: the guard has to be a *total* order, and a
    /// comparator that only handled `(Some, None)` would pass a one-directional assertion while
    /// still inverting the other half of the sort.
    #[test]
    fn an_unpriced_model_compares_after_a_priced_one() {
        use std::cmp::Ordering;

        assert_eq!(cmp_price(Some(10), Some(20)), Ordering::Less);
        assert_eq!(cmp_price(Some(20), Some(10)), Ordering::Greater);
        assert_eq!(cmp_price(Some(10), Some(10)), Ordering::Equal);

        // The two the derived `Option::cmp` gets backwards.
        assert_eq!(cmp_price(Some(10), None), Ordering::Less);
        assert_eq!(cmp_price(None, Some(10)), Ordering::Greater);
        assert_eq!(
            cmp_price(None, None),
            Ordering::Equal,
            "two unpriced rows tie; the caller breaks the tie by model key"
        );

        // Proof the trap is real, so nobody "simplifies" this back into `a.cmp(&b)` and keeps
        // a green test suite: this is what the derived ordering actually does.
        assert_eq!(None.cmp(&Some(10)), Ordering::Less);

        // And it agrees with what the SQL path says, in the direction that matters.
        assert!(
            CatalogSort::Price.order_by().contains("nulls last"),
            "the SQL fragment and cmp_price must express one rule"
        );
    }

    #[test]
    fn the_requirement_vocabulary_is_exactly_four() {
        assert_eq!(
            ROUTE_REQUIREMENTS,
            &["tools", "vision", "long_context", "json"]
        );
        for requirement in ROUTE_REQUIREMENTS {
            validate_requirement(requirement).expect("known");
        }
        assert!(validate_requirement("speed").is_err());
    }

    #[test]
    fn the_price_source_vocabulary_round_trips() {
        for source in PriceSource::ALL {
            assert_eq!(PriceSource::parse(source.as_str()), Some(*source));
            assert!(!source.note().is_empty());
        }
        assert_eq!(PriceSource::parse("scraped"), None);
    }

    // ---------------------------------------------------------------------------------------
    // Prices
    // ---------------------------------------------------------------------------------------

    #[test]
    fn an_absent_price_is_unknown_rather_than_free() {
        let price = ModelPrice::unknown();
        assert!(!price.is_known());
        assert!(!price.is_complete());
        // The important half: a cost estimate cannot be built from nothing, and returns None
        // rather than a zero that reads as "this call was free".
        assert_eq!(price.cost_micros(1_000, 1_000), None);
    }

    #[test]
    fn a_half_priced_model_has_an_unknown_cost_not_a_cheap_one() {
        let price = ModelPrice {
            input_micros_per_mtok: Some(1_000),
            output_micros_per_mtok: None,
            source: PriceSource::Manual,
            updated_at: None,
        };
        assert!(price.is_known());
        assert!(!price.is_complete());
        // The known input half must NOT be returned on its own: understating every call is the
        // exact quiet wrongness this request forbids.
        assert_eq!(price.cost_micros(1_000_000, 500_000), None);
    }

    #[test]
    fn a_complete_price_costs_what_the_arithmetic_says() {
        let price = ModelPrice {
            input_micros_per_mtok: Some(1_000),
            output_micros_per_mtok: Some(2_000),
            source: PriceSource::Manual,
            updated_at: None,
        };
        // 1M input at 1000 micros/MTok = 1000 micros; 500k output at 2000 = 1000 micros.
        assert_eq!(price.cost_micros(1_000_000, 500_000), Some(2_000));
        assert_eq!(price.cost_micros(0, 0), Some(0));
    }

    #[test]
    fn a_negative_token_count_never_becomes_a_credit() {
        let price = ModelPrice {
            input_micros_per_mtok: Some(1_000),
            output_micros_per_mtok: Some(1_000),
            source: PriceSource::Manual,
            updated_at: None,
        };
        assert_eq!(price.cost_micros(-1_000, -1_000), Some(0));
    }

    #[test]
    fn the_per_1k_rendering_rounds_up_so_a_real_cost_never_reads_as_zero() {
        let price = ModelPrice {
            // One micro per million tokens: a hundredth of a micro per 1K.
            input_micros_per_mtok: Some(1),
            output_micros_per_mtok: Some(0),
            source: PriceSource::Manual,
            updated_at: None,
        };
        assert_eq!(
            price.per_1k_micros(PriceHalf::Input),
            Some(1),
            "a non-zero rate must not print as 0"
        );
        assert_eq!(
            price.per_1k_micros(PriceHalf::Output),
            Some(0),
            "a genuinely free half is 0"
        );
    }

    #[test]
    fn a_bad_price_names_both_halves_at_once() {
        assert!(validate_price(Some(1_000), Some(2_000)).is_ok());
        assert!(validate_price(None, None).is_ok());
        assert!(validate_price(Some(0), None).is_err());
        assert!(validate_price(None, Some(-1)).is_err());

        let both = validate_price(Some(-1), Some(0)).expect_err("both halves wrong");
        let message = both.to_string();
        assert!(message.contains("input"), "{message}");
        assert!(message.contains("output"), "{message}");
    }

    #[test]
    fn a_megabyte_price_is_refused_rather_than_stored() {
        let refused = validate_price(Some(MICROS_CEILING_PER_MTOK + 1), None)
            .expect_err("a typo must not become a cost report");
        assert!(
            refused.to_string().contains("micros per million"),
            "{refused}"
        );
        assert!(validate_price(Some(MICROS_CEILING_PER_MTOK), None).is_ok());
    }

    #[test]
    fn the_price_view_carries_both_precisions_of_the_same_number() {
        let price = ModelPrice {
            input_micros_per_mtok: Some(15_000),
            output_micros_per_mtok: Some(60_000),
            source: PriceSource::Probe,
            updated_at: None,
        };
        let view = PriceView::from(&price);
        assert_eq!(view.input_micros_per_mtok, Some(15_000));
        assert_eq!(view.input_micros_per_1k, Some(15));
        assert_eq!(view.output_micros_per_1k, Some(60));
        assert_eq!(view.source, PriceSource::Probe);
        assert!(view.complete);
        assert!(!view.source_note.is_empty());
    }

    #[test]
    fn a_price_gets_old_and_is_flagged() {
        let now = datetime!(2026-09-28 12:00 UTC);
        let fresh = now - time::Duration::days(10);
        let old = now - time::Duration::days(200);
        let never = None;

        assert_eq!(price_age_days(Some(fresh), now), Some(10));
        assert!(!price_is_stale(Some(fresh), now));
        assert!(price_is_stale(Some(old), now));
        // Never written is not "zero days old" — it renders as "no price", not as fresh.
        assert_eq!(price_age_days(never, now), None);
        assert!(!price_is_stale(never, now));
    }

    // ---------------------------------------------------------------------------------------
    // Capability filter
    // ---------------------------------------------------------------------------------------

    #[test]
    fn a_capability_filter_is_a_conjunction_not_a_disjunction() {
        let plain = model("plain");
        let mut rich = model("rich");
        rich.supports_tools = true;
        rich.supports_vision = true;

        let both = CapabilityFilter::new(vec![ModelCapability::Tools, ModelCapability::Vision]);
        assert!(!both.matches(&plain));
        assert!(both.matches(&rich));
        // The disjunction reading would have let `plain` through on "it has streaming", which is
        // the confusion the filter exists to remove.
        assert_eq!(both.apply(&[plain.clone(), rich.clone()]).len(), 1);
    }

    #[test]
    fn a_cleared_filter_passes_everything() {
        let filter = CapabilityFilter::default();
        let models = vec![model("a"), model("b")];
        assert_eq!(filter.apply(&models).len(), 2);
    }

    // ---------------------------------------------------------------------------------------
    // Task compatibility
    // ---------------------------------------------------------------------------------------

    #[test]
    fn only_an_embedding_model_can_serve_the_embedding_task() {
        // This is the acceptance criterion stated by name: a model with
        // supports_embeddings = false may not be chosen for an embedding task.
        let mut chatty = model("chatty");
        assert!(!can_serve_task("embedding", &chatty));
        let refusal = task_refusal_reason("embedding", &chatty).expect("refused");
        assert!(refusal.contains("embeddings flag"), "{refusal}");

        chatty.supports_embeddings = true;
        assert!(can_serve_task("embedding", &chatty));
        assert_eq!(task_refusal_reason("embedding", &chatty), None);
    }

    #[test]
    fn the_vision_task_needs_the_vision_flag_and_the_coding_task_needs_tools() {
        let mut plain = model("plain");
        assert!(!can_serve_task("vision", &plain));
        assert!(!can_serve_task("coding", &plain));
        assert!(!can_serve_task("critical", &plain));

        plain.supports_vision = true;
        assert!(can_serve_task("vision", &plain));
        assert!(
            can_serve_task("cheap", &plain),
            "cheap has no structural need"
        );
        assert!(can_serve_task("translation", &plain));
    }

    #[test]
    fn a_long_context_route_wants_a_real_window_and_says_so_when_there_is_none() {
        let mut small = model("small");
        small.context_window = Some(8_000);
        let refusal = task_refusal_reason("long_context", &small).expect("refused");
        assert!(refusal.contains("8000"), "{refusal}");

        let unknown = model("unknown");
        let refusal = task_refusal_reason("long_context", &unknown).expect("refused");
        assert!(
            refusal.contains("no context window"),
            "an unknown window is not a big one: {refusal}"
        );

        let mut big = model("big");
        big.context_window = Some(LONG_CONTEXT_TOKENS);
        assert!(can_serve_task("long_context", &big));
    }

    #[test]
    fn a_requirement_that_fails_names_the_capability() {
        let plain = model("plain");
        let refusal = requirement_refusal_reason("tools", &plain).expect("refused");
        assert!(refusal.contains("tools"), "{refusal}");
        assert_eq!(
            requirement_refusal_reason("json", &plain).map(|r| r.contains("json")),
            Some(true)
        );

        let mut rich = model("rich");
        rich.supports_tools = true;
        rich.supports_json_mode = true;
        rich.supports_vision = true;
        // The window matters: `long_context` is a fact about the context size rather than a
        // stored flag, so a model with no recorded window must fail that requirement even though
        // it claims all three capabilities. Giving it a real one is what makes this the "a
        // satisfied requirement passes" case rather than a second copy of the refusal test.
        rich.context_window = Some(LONG_CONTEXT_TOKENS);
        for requirement in ROUTE_REQUIREMENTS {
            assert!(requirement_refusal_reason(requirement, &rich).is_none());
        }

        // And the asymmetry is deliberate: drop the window and the other three still pass while
        // this one does not.
        rich.context_window = None;
        for requirement in ["tools", "vision", "json"] {
            assert!(requirement_refusal_reason(requirement, &rich).is_none());
        }
        assert!(requirement_refusal_reason("long_context", &rich).is_some());
    }

    #[test]
    fn an_unknown_requirement_is_refused_rather_than_ignored() {
        // Silently passing a requirement the platform cannot evaluate would let a route claim a
        // guarantee it never checked.
        let plain = model("plain");
        assert!(requirement_refusal_reason("speed", &plain).is_some());
    }

    // ---------------------------------------------------------------------------------------
    // Usage
    // ---------------------------------------------------------------------------------------

    #[test]
    fn a_model_nothing_points_at_is_unreferenced() {
        let usage = ModelUsage::default();
        assert!(usage.is_unreferenced());
        assert_eq!(usage.reference_count(), 0);

        let routed = ModelUsage {
            tasks: vec!["cheap".to_owned()],
            ..ModelUsage::default()
        };
        assert!(!routed.is_unreferenced());
        assert_eq!(routed.reference_count(), 1);

        let defaulted = ModelUsage {
            is_default: true,
            ..ModelUsage::default()
        };
        assert!(!defaulted.is_unreferenced(), "the default is a reference");
        assert_eq!(defaulted.reference_count(), 1);
    }

    #[test]
    fn today_comes_from_the_clock_not_from_the_database() {
        let now = datetime!(2026-09-28 23:30 UTC);
        assert_eq!(
            today(now),
            Date::from_calendar_date(2026, time::Month::September, 28).unwrap()
        );
    }

    // ---------------------------------------------------------------------------------------
    // The catalog query
    // ---------------------------------------------------------------------------------------

    #[test]
    fn search_matches_the_key_the_label_and_the_provider() {
        // The acceptance criterion names all three fields, so all three are asserted by name.
        let mut plain = model("gpt-4o-mini");
        plain.display_name = Some("Small General".to_owned());

        let by_key = CatalogQuery {
            q: Some("4O-M".to_owned()),
            ..CatalogQuery::default()
        };
        assert!(by_key.matches(&plain, "Office AI"), "case-insensitive key");

        let by_label = CatalogQuery {
            q: Some("general".to_owned()),
            ..CatalogQuery::default()
        };
        assert!(by_label.matches(&plain, "Office AI"), "display name");

        let by_provider = CatalogQuery {
            q: Some("office".to_owned()),
            ..CatalogQuery::default()
        };
        assert!(by_provider.matches(&plain, "Office AI"), "provider name");

        let miss = CatalogQuery {
            q: Some("absent".to_owned()),
            ..CatalogQuery::default()
        };
        assert!(!miss.matches(&plain, "Office AI"));
    }

    #[test]
    fn a_blank_search_is_no_filter_at_all() {
        let plain = model("m");
        for blank in [None, Some(String::new()), Some("   ".to_owned())] {
            let query = CatalogQuery {
                q: blank,
                ..CatalogQuery::default()
            };
            assert!(
                query.matches(&plain, "P"),
                "a blank search must not empty the table"
            );
        }
    }

    #[test]
    fn a_capability_filter_narrows_the_table_and_an_unknown_flag_is_refused() {
        let mut rich = model("rich");
        rich.supports_tools = true;
        let plain = model("plain");

        let required = CatalogQuery {
            capabilities: vec![ModelCapability::Tools],
            ..CatalogQuery::default()
        };
        assert!(required.matches(&rich, "P"));
        assert!(!required.matches(&plain, "P"));

        let parsed = CatalogQuery::capabilities_from_param(Some("tools, vision")).expect("both");
        assert_eq!(parsed.len(), 2);
        assert!(
            CatalogQuery::capabilities_from_param(None)
                .expect("none")
                .is_empty()
        );
        assert!(
            CatalogQuery::capabilities_from_param(Some(""))
                .expect("empty")
                .is_empty()
        );

        // Silently dropping the unknown key would leave a table narrowed by one capability while
        // the operator believed it was narrowed by two.
        let refused = CatalogQuery::capabilities_from_param(Some("tools,telepathy"))
            .expect_err("unknown flag");
        assert!(refused.to_string().contains("tools"), "{refused}");
    }

    #[test]
    fn the_status_filter_separates_enabled_from_disabled() {
        let mut off = model("off");
        off.enabled = false;
        let on = model("on");

        let enabled = CatalogQuery {
            status: Some(true),
            ..CatalogQuery::default()
        };
        assert!(enabled.matches(&on, "P"));
        assert!(!enabled.matches(&off, "P"));

        let disabled = CatalogQuery {
            status: Some(false),
            ..CatalogQuery::default()
        };
        assert!(disabled.matches(&off, "P"));
        assert!(!disabled.matches(&on, "P"));

        let both = CatalogQuery::default();
        assert!(both.matches(&on, "P") && both.matches(&off, "P"));
    }

    #[test]
    fn a_provider_filter_keeps_one_providers_rows_apart() {
        let first = model("shared");
        let mut second = model("shared");
        second.provider_id = Uuid::new_v4();

        let only_second = CatalogQuery::default().for_provider(second.provider_id);
        assert!(!only_second.matches(&first, "P"));
        assert!(only_second.matches(&second, "P"));
    }

    #[test]
    fn a_sort_key_is_a_whitelist_and_an_unknown_one_falls_back() {
        for sort in CatalogSort::ALL {
            assert_eq!(CatalogSort::parse(sort.as_str()), *sort);
            assert!(!sort.order_by().is_empty());
        }
        // A stale bookmark must not turn a working table into an error page.
        assert_eq!(CatalogSort::parse("by_vibes"), CatalogSort::Model);
        assert_eq!(CatalogSort::default(), CatalogSort::Model);
    }

    #[test]
    fn sorting_puts_an_unknown_context_window_last_not_first() {
        // The SQL fragment says `nulls last`; asserting the *intent* here means a future edit
        // that drops the clause fails in this file first, where the reason is written down.
        assert!(CatalogSort::Context.order_by().contains("nulls last"));
        assert!(CatalogSort::Price.order_by().contains("nulls last"));

        let mut known = model("known");
        known.context_window = Some(8_000);
        let unknown = model("unknown");
        assert!(known.context_window.is_some());
        assert!(unknown.context_window.is_none());
    }

    #[test]
    fn a_catalog_entry_names_its_pair_the_one_way() {
        let mut plain = model("gpt-4o-mini");
        plain.display_name = Some("Small".to_owned());
        plain.context_window = Some(128_000);
        plain.input_cost_micros_per_mtok = Some(15_000);
        plain.output_cost_micros_per_mtok = Some(60_000);

        let entry = catalog_entry(&plain, "Office AI");
        assert_eq!(entry.pair_id, "Office AI/gpt-4o-mini");
        assert_eq!(entry.display_name, "Small");
        assert_eq!(entry.context_window, Some(128_000));
        assert_eq!(entry.price.input_micros_per_1k, Some(15));
        assert!(entry.capabilities.contains(&"chat".to_owned()));
        assert!(entry.capabilities.contains(&"streaming".to_owned()));
        assert!(!entry.capabilities.contains(&"vision".to_owned()));
        // Nothing routes here yet, and saying so is what keeps the "used by" column honest
        // before slice 2 lands.
        assert!(entry.usage.is_unreferenced());
    }

    #[test]
    fn the_registries_carry_their_own_descriptions_in_a_stable_order() {
        let features = model_features();
        assert_eq!(features.len(), MODEL_FEATURES.len());
        assert_eq!(features[0].key, "content_assist");
        for entry in &features {
            assert!(!entry.description.is_empty(), "{} needs prose", entry.key);
        }

        let tasks = routing_tasks();
        assert_eq!(tasks[0].key, "cheap");
        assert_eq!(tasks.len(), 7);
    }
}
