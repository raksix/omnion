//! Deals, pipelines and the board they are shown on (docs/requests/REQ-051, slice 3).
//!
//! A pipeline is a set of **ordered stages**; a stage is one of three kinds — `open`, `won`,
//! `lost` — and the kinds are what make the board more than a set of buckets: the open stages
//! carry the forecast, and a deal that reaches a `lost` stage has to say why.
//!
//! Three rules hold here, and each of them is a rule the **schema** also has to hold, which is
//! why they are written once in Rust and once in SQL:
//!
//! * **The stage must belong to the pipeline.** A card in a column of another pipeline would
//!   sit inside another pipeline's totals.
//! * **A lost deal says why, and leaving the lost column forgets it.** A reason that outlives the
//!   loss would credit the next report with the wrong deal.
//! * **A won deal records a close date.** The board's "won this quarter" is a date query, and a
//!   won deal without one is not in it.
//!
//! The board reads in **one request**: the stages, the cards and the per-stage totals come from
//! `board_payload`, and every total is one SQL expression (`sum(amount)` and
//! `sum(amount * probability / 100)`) so a column header and a card in it can never disagree.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use sqlx::postgres::PgQueryResult;
use sqlx::QueryBuilder;
use time::{Date, OffsetDateTime};
use uuid::Uuid;

use crate::error::{CrmError, Result};
use crate::model::clean;
use crate::query::{ListQuery, Page, Scope, next_cursor, now};

// ---------------------------------------------------------------------------------------------
// Vocabulary
// ---------------------------------------------------------------------------------------------

/// What a stage is for: a step, an outcome won, or an outcome lost.
pub const STAGE_KINDS: [&str; 3] = ["open", "won", "lost"];

/// Currencies the deal form offers, and the default of a deal that names none.
pub const CURRENCIES: [&str; 8] = ["USD", "EUR", "GBP", "TRY", "CHF", "CAD", "AUD", "JPY"];

/// Longest a deal title may be.
pub const MAX_TITLE_LENGTH: usize = 200;

/// Longest a lost reason may be.
pub const MAX_LOST_REASON_LENGTH: usize = 200;

/// Longest a source note may be.
pub const MAX_SOURCE_LENGTH: usize = 120;

/// How many stages a pipeline may have, so a board stays a board and not a wall of columns.
pub const MAX_STAGES: usize = 24;

/// Days after which a card is drawn as stale on the board.
pub const STALE_AFTER_DAYS: i64 = 30;

/// `true` when the value is one of the three stage kinds.
#[must_use]
pub fn is_stage_kind(value: &str) -> bool {
    STAGE_KINDS.contains(&value.trim())
}

/// The weighted forecast of one deal: `amount × probability / 100`.
///
/// Written here as the documented rule so a unit test can pin it, and **also** as one SQL
/// expression in [`stage_totals`] — the Rust version is what the form previews, the SQL version
/// is what the column header shows, and the two must not drift apart.
#[must_use]
pub fn weighted_amount(amount: &str, probability: i32) -> String {
    let amount: f64 = amount.trim().parse().unwrap_or(0.0);
    format!("{:.2}", amount * f64::from(probability) / 100.0)
}

/// How long a deal has sat in its stage, in whole days.
#[must_use]
pub fn days_in_stage(stage_changed_at: OffsetDateTime, at: OffsetDateTime) -> i64 {
    let elapsed = at - stage_changed_at;
    if elapsed.is_negative() { 0 } else { elapsed.whole_days() }
}

/// `true` when a card has been in its stage longer than [`STALE_AFTER_DAYS`].
#[must_use]
pub fn is_stale(stage_changed_at: OffsetDateTime, at: OffsetDateTime) -> bool {
    days_in_stage(stage_changed_at, at) > STALE_AFTER_DAYS
}

// ---------------------------------------------------------------------------------------------
// Read shapes
// ---------------------------------------------------------------------------------------------

/// One stage of a pipeline.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PipelineStage {
    /// Identifier.
    pub id: Uuid,
    /// Owning organization.
    pub organization_id: Uuid,
    /// The pipeline it belongs to.
    pub pipeline_id: Uuid,
    /// Column name.
    pub name: String,
    /// `open`, `won` or `lost`.
    pub kind: String,
    /// Order on the board.
    pub position: i32,
    /// Default probability a deal entering this stage gets.
    pub probability: i32,
}

/// A pipeline with its stages in board order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Pipeline {
    /// Identifier.
    pub id: Uuid,
    /// Owning organization.
    pub organization_id: Uuid,
    /// Display name.
    pub name: String,
    /// The pipeline a new deal lands on when the caller names none.
    pub is_default: bool,
    /// The stages, ordered.
    pub stages: Vec<PipelineStage>,
    /// When it was created.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// The per-column header of the board: the count, the sum and the weighted sum.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StageTotals {
    /// The stage the column is.
    pub stage_id: Uuid,
    /// Its name.
    pub name: String,
    /// Its kind.
    pub kind: String,
    /// Its position.
    pub position: i32,
    /// Its default probability.
    pub probability: i32,
    /// How many live deals sit in it.
    pub deal_count: i64,
    /// The sum of their amounts, as text so a money value never loses precision in JSON.
    pub total: String,
    /// The sum of `amount × probability / 100`, in the same shape.
    pub weighted_total: String,
}

/// A deal as a board card, a list row and a detail header.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Deal {
    /// Identifier.
    pub id: Uuid,
    /// Owning organization.
    pub organization_id: Uuid,
    /// The pipeline it is on.
    pub pipeline_id: Uuid,
    /// The stage it sits in.
    pub stage_id: Uuid,
    /// The stage's name, joined for the board and the list.
    pub stage_name: String,
    /// The stage's kind, joined — the board needs it to draw the won/lost columns.
    pub stage_kind: String,
    /// Headline.
    pub title: String,
    /// Company, joined.
    pub company_id: Option<Uuid>,
    /// Company name, joined.
    pub company_name: Option<String>,
    /// Contact, joined.
    pub contact_id: Option<Uuid>,
    /// Contact display name, joined.
    pub contact_name: Option<String>,
    /// Owner.
    pub owner_user_id: Option<Uuid>,
    /// Owner display name, joined.
    pub owner_name: Option<String>,
    /// Deal value, as text.
    pub amount: String,
    /// ISO 4217 code.
    pub currency: String,
    /// Win probability as a percentage, or `None` when the stage's own is used.
    pub probability: Option<i32>,
    /// When it is expected to close.
    #[serde(default, with = "crate::dates::option")]
    pub expected_close_on: Option<Date>,
    /// Where it came from.
    pub source: Option<String>,
    /// Why it was lost; present only while the deal sits in a `lost` stage.
    pub lost_reason: Option<String>,
    /// When it last changed stage.
    #[serde(with = "time::serde::rfc3339")]
    pub stage_changed_at: OffsetDateTime,
    /// Whole days in the current stage.
    pub days_in_stage: i64,
    /// `true` once a card has been in its stage longer than [`STALE_AFTER_DAYS`].
    pub stale: bool,
    /// When it was archived.
    #[serde(with = "time::serde::rfc3339::option")]
    pub archived_at: Option<OffsetDateTime>,
    /// When it was created.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// When it last changed.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

/// The board in one request: the pipeline, its stages, the columns' totals and the cards.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Board {
    /// The pipeline the board is showing.
    pub pipeline: Pipeline,
    /// One entry per stage, in board order — including the empty ones, because a column with
    /// no cards is still a column a person needs to see to know where the board ends.
    pub columns: Vec<StageTotals>,
    /// The live deals, in stage order.
    pub deals: Vec<Deal>,
    /// The sum of every **open** column, as text.
    pub open_total: String,
    /// The sum of every **open** column weighted by probability, as text.
    pub weighted_forecast: String,
}

/// Row shape of the deal query; every join is a left join so an unowned deal still comes back.
#[derive(Debug, sqlx::FromRow)]
struct DealRow {
    id: Uuid,
    organization_id: Uuid,
    pipeline_id: Uuid,
    stage_id: Uuid,
    stage_name: String,
    stage_kind: String,
    title: String,
    company_id: Option<Uuid>,
    company_name: Option<String>,
    contact_id: Option<Uuid>,
    contact_name: Option<String>,
    owner_user_id: Option<Uuid>,
    owner_name: Option<String>,
    amount: String,
    currency: String,
    probability: Option<i32>,
    expected_close_on: Option<Date>,
    source: Option<String>,
    lost_reason: Option<String>,
    stage_changed_at: OffsetDateTime,
    archived_at: Option<OffsetDateTime>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl DealRow {
    fn into_deal(self, at: OffsetDateTime) -> Deal {
        let days = days_in_stage(self.stage_changed_at, at);
        Deal {
            id: self.id,
            organization_id: self.organization_id,
            pipeline_id: self.pipeline_id,
            stage_id: self.stage_id,
            stage_name: self.stage_name,
            stage_kind: self.stage_kind,
            title: self.title,
            company_id: self.company_id,
            company_name: self.company_name,
            contact_id: self.contact_id,
            contact_name: self.contact_name,
            owner_user_id: self.owner_user_id,
            owner_name: self.owner_name,
            amount: self.amount,
            currency: self.currency,
            probability: self.probability,
            expected_close_on: self.expected_close_on,
            source: self.source,
            lost_reason: self.lost_reason,
            stage_changed_at: self.stage_changed_at,
            days_in_stage: days,
            stale: days > STALE_AFTER_DAYS,
            archived_at: self.archived_at,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

/// The columns the deal query selects, with the joins the board needs.
const DEAL_SELECT: &str = "select d.id, d.organization_id, d.pipeline_id, d.stage_id, \
     s.name as stage_name, s.kind as stage_kind, d.title, d.company_id, co.name as company_name, \
     d.contact_id, case when c.id is null then null \
       else btrim(coalesce(c.first_name, '') || ' ' || coalesce(c.last_name, '')) end as contact_name, \
     d.owner_user_id, u.display_name as owner_name, d.amount::text as amount, d.currency, \
     d.probability, d.expected_close_on, d.source, d.lost_reason, d.stage_changed_at, \
     d.archived_at, d.created_at, d.updated_at \
     from crm_deals d \
     join crm_pipeline_stages s on s.id = d.stage_id \
     left join crm_companies co on co.id = d.company_id \
     left join crm_contacts c on c.id = d.contact_id \
     left join users u on u.id = d.owner_user_id";

// ---------------------------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------------------------

/// A deal as the create form describes it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct DealChanges {
    /// Headline (required).
    pub title: String,
    /// Pipeline; the organization's default when absent.
    pub pipeline_id: Option<Uuid>,
    /// Stage; the pipeline's first open stage when absent.
    pub stage_id: Option<Uuid>,
    /// Company.
    pub company_id: Option<Uuid>,
    /// Contact.
    pub contact_id: Option<Uuid>,
    /// Owner (defaults to the caller at the route layer).
    pub owner_user_id: Option<Uuid>,
    /// Value, as text so the form can send what it displays.
    pub amount: Option<String>,
    /// ISO 4217 code.
    pub currency: Option<String>,
    /// Win probability.
    pub probability: Option<i32>,
    /// Expected close date.
    #[serde(default, with = "crate::dates::option")]
    pub expected_close_on: Option<Date>,
    /// Where it came from.
    pub source: Option<String>,
    /// Why it was lost — required when the chosen stage is a `lost` one.
    pub lost_reason: Option<String>,
}

/// A partial update of a deal's own fields (the stage moves through [`StageMove`]).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct DealPatch {
    /// Headline.
    pub title: Option<String>,
    /// Company.
    pub company_id: Option<Uuid>,
    /// Contact.
    pub contact_id: Option<Uuid>,
    /// Owner.
    pub owner_user_id: Option<Uuid>,
    /// Value.
    pub amount: Option<String>,
    /// Currency.
    pub currency: Option<String>,
    /// Win probability.
    pub probability: Option<i32>,
    /// Expected close date.
    #[serde(default, with = "crate::dates::option")]
    pub expected_close_on: Option<Date>,
    /// Source.
    pub source: Option<String>,
}

/// A stage move: the drag, and the `ctrl + ←/→` the keyboard sends.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct StageMove {
    /// The stage the deal goes to.
    pub stage_id: Uuid,
    /// Required when the target stage is a `lost` one.
    #[serde(default)]
    pub lost_reason: Option<String>,
    /// Confirm when the target stage is a `won` one; today when absent.
    #[serde(default, with = "crate::dates::option")]
    pub close_on: Option<Date>,
}

/// One stage of the pipeline editor.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct StageChanges {
    /// Column name.
    pub name: String,
    /// `open`, `won` or `lost`.
    #[serde(default)]
    pub kind: Option<String>,
    /// Default probability.
    #[serde(default)]
    pub probability: Option<i32>,
}

/// The pipeline editor's save: the whole ordered set, because a drag reorder is the set itself.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct StageSet {
    /// The stages, in board order.
    pub stages: Vec<StageChanges>,
}

/// A deal that passed validation.
#[derive(Debug, Clone, PartialEq)]
pub struct NormalisedDeal {
    /// Headline.
    pub title: String,
    /// Value.
    pub amount: String,
    /// Currency.
    pub currency: String,
    /// Probability.
    pub probability: Option<i32>,
    /// Expected close date.
    pub expected_close_on: Option<Date>,
    /// Source.
    pub source: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Validation — pure, so a screen's refusal is provable without a database
// ---------------------------------------------------------------------------------------------

/// Validate a deal description, naming the field that failed.
pub fn validate_deal(changes: &DealChanges) -> Result<NormalisedDeal> {
    let title = changes.title.trim().to_owned();
    if title.is_empty() {
        return Err(CrmError::invalid("deal", "title", "a deal needs a title"));
    }
    if title.chars().count() > MAX_TITLE_LENGTH {
        return Err(CrmError::invalid(
            "deal",
            "title",
            format!("a deal title is at most {MAX_TITLE_LENGTH} characters"),
        ));
    }

    let amount = validate_amount(changes.amount.as_deref())?;
    let currency = validate_currency(changes.currency.as_deref())?;
    validate_probability(changes.probability, "deal")?;

    let source = clean(changes.source.clone());
    if source.as_ref().is_some_and(|text| text.chars().count() > MAX_SOURCE_LENGTH) {
        return Err(CrmError::invalid(
            "deal",
            "source",
            format!("a source is at most {MAX_SOURCE_LENGTH} characters"),
        ));
    }

    Ok(NormalisedDeal {
        title,
        amount,
        currency,
        probability: changes.probability,
        expected_close_on: changes.expected_close_on,
        source,
    })
}

/// Validate a money value: a non-negative decimal, and never one precision's worth of noise
/// beyond the schema's `numeric(14,2)`.
pub fn validate_amount(raw: Option<&str>) -> Result<String> {
    let Some(text) = clean(raw.map(str::to_owned)) else {
        return Ok("0.00".to_owned());
    };
    // Both separators are accepted: a form in a Turkish locale sends `1.234,56`, and a person
    // typing into a number field should not have to know which one the API wants.
    let normalised = normalise_number(&text);
    let parsed: f64 = normalised.parse().map_err(|_| {
        CrmError::invalid("deal", "amount", "a value is a number such as 12500 or 12500.00")
    })?;
    if !parsed.is_finite() || parsed < 0.0 {
        return Err(CrmError::invalid("deal", "amount", "a value is zero or more"));
    }
    if parsed >= 1.0e12 {
        return Err(CrmError::invalid(
            "deal",
            "amount",
            "a value is at most 999999999999.99",
        ));
    }
    Ok(format!("{parsed:.2}"))
}

/// Strip thousands separators and normalise the decimal mark, refusing a stray letter.
pub fn normalise_number(raw: &str) -> String {
    let mut body = raw.trim().to_owned();
    if body.contains(',') && body.contains('.') {
        // Whichever comes last is the decimal mark.
        let decimal_at = body.rfind(['.', ',']).unwrap_or(0);
        let (head, tail) = body.split_at(decimal_at);
        let cleaned_head: String = head.chars().filter(|c| c.is_ascii_digit()).collect();
        let cleaned_tail: String = tail[1..].chars().filter(|c| c.is_ascii_digit()).collect();
        body = format!("{cleaned_head}.{cleaned_tail}");
    } else if let Some(index) = body.find(',') {
        let (head, tail) = body.split_at(index);
        let cleaned_tail: String = tail[1..].chars().filter(|c| c.is_ascii_digit()).collect();
        body = if cleaned_tail.len() == 3 && tail.len() == 4 {
            // `1,234` is a thousand separator; `12,5` is a decimal comma.
            format!("{}{cleaned_tail}", head.chars().filter(|c| c.is_ascii_digit()).collect::<String>())
        } else {
            format!("{}.{cleaned_tail}", head.chars().filter(|c| c.is_ascii_digit()).collect::<String>())
        };
    }
    body
}

/// Validate a currency code: three uppercase letters the form offers.
pub fn validate_currency(raw: Option<&str>) -> Result<String> {
    let Some(code) = clean(raw.map(str::to_owned)) else {
        return Ok("USD".to_owned());
    };
    let code = code.to_uppercase();
    if !CURRENCIES.contains(&code.as_str()) {
        return Err(CrmError::invalid(
            "deal",
            "currency",
            format!("a currency is one of {}", CURRENCIES.join(", ")),
        ));
    }
    Ok(code)
}

/// Validate a probability, refusing anything outside 0–100.
pub fn validate_probability(value: Option<i32>, entity: &'static str) -> Result<Option<i32>> {
    match value {
        Some(number) if !(0..=100).contains(&number) => Err(CrmError::invalid(
            entity,
            "probability",
            "a probability is between 0 and 100",
        )),
        other => Ok(other),
    }
}

/// Validate a lost reason: it is the *only* thing that explains a lost column.
pub fn validate_lost_reason(raw: Option<&str>) -> Result<Option<String>> {
    let Some(reason) = clean(raw.map(str::to_owned)) else {
        return Ok(None);
    };
    if reason.chars().count() > MAX_LOST_REASON_LENGTH {
        return Err(CrmError::invalid(
            "deal",
            "lost_reason",
            format!("a lost reason is at most {MAX_LOST_REASON_LENGTH} characters"),
        ));
    }
    Ok(Some(reason))
}

/// What a stage move decided, after the target stage's kind was read.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedMove {
    /// The reason to store, or `None` to clear the old one.
    pub lost_reason: Option<String>,
    /// The close date to store, or `None` to leave the deal's own expectation alone.
    pub close_on: Option<Date>,
}

/// The pure half of a stage move: what the target stage's kind demands.
///
/// `today` is passed in rather than read from the clock so the rule is provable, and so a won
/// deal's close date is the day it was actually won rather than the day a test ran.
pub fn resolve_move(
    move_to: &StageMove,
    stage_kind: &str,
    today: Date,
) -> Result<ResolvedMove> {
    match stage_kind {
        "lost" => {
            let reason = validate_lost_reason(move_to.lost_reason.as_deref())?.ok_or_else(|| {
                CrmError::InvalidStageChange(
                    "a deal cannot move to a lost stage without a reason".to_owned(),
                )
            })?;
            Ok(ResolvedMove {
                lost_reason: Some(reason),
                close_on: None,
            })
        }
        "won" => Ok(ResolvedMove {
            // Leaving the lost column forgets the reason: the `crm_deals_lost_reason` trigger
            // clears it in SQL, and the API clears it here so the returned row is what a
            // reload would show.
            lost_reason: None,
            close_on: Some(move_to.close_on.unwrap_or(today)),
        }),
        _ => Ok(ResolvedMove {
            lost_reason: None,
            close_on: None,
        }),
    }
}

/// Validate one row of the pipeline editor.
pub fn validate_stage(entry: &StageChanges) -> Result<(String, String, i32)> {
    let name = entry.name.trim().to_owned();
    if name.is_empty() {
        return Err(CrmError::invalid("stage", "name", "a stage needs a name"));
    }
    if name.chars().count() > 80 {
        return Err(CrmError::invalid(
            "stage",
            "name",
            "a stage name is at most 80 characters",
        ));
    }
    let kind = clean(entry.kind.clone()).unwrap_or_else(|| "open".to_owned()).to_lowercase();
    if !is_stage_kind(&kind) {
        return Err(CrmError::invalid(
            "stage",
            "kind",
            "a stage is open, won or lost",
        ));
    }
    let probability = validate_probability(Some(entry.probability.unwrap_or(0)), "stage")?
        .unwrap_or_default();
    // A won column's probability is the forecast itself: leaving it at 0 would make every won
    // deal contribute nothing to the weighted total of an open board.
    let probability = if kind == "won" { 100 } else { probability };
    Ok((name, kind, probability))
}

/// Validate the pipeline editor's whole set: the kinds, the positions and the one-outcome rule.
pub fn validate_stage_set(set: &StageSet) -> Result<Vec<(String, String, i32)>> {
    if set.stages.is_empty() {
        return Err(CrmError::invalid(
            "pipeline",
            "stages",
            "a pipeline needs at least one stage",
        ));
    }
    if set.stages.len() > MAX_STAGES {
        return Err(CrmError::invalid(
            "pipeline",
            "stages",
            format!("a pipeline has at most {MAX_STAGES} stages"),
        ));
    }

    let mut validated = Vec::with_capacity(set.stages.len());
    for entry in &set.stages {
        validated.push(validate_stage(entry)?);
    }

    let open = validated.iter().filter(|(_, kind, _)| kind == "open").count();
    let won = validated.iter().filter(|(_, kind, _)| kind == "won").count();
    let lost = validated.iter().filter(|(_, kind, _)| kind == "lost").count();
    if open == 0 {
        return Err(CrmError::invalid(
            "pipeline",
            "stages",
            "a pipeline needs at least one open stage",
        ));
    }
    if won > 1 {
        return Err(CrmError::invalid(
            "pipeline",
            "stages",
            "a pipeline may have at most one won stage",
        ));
    }
    if lost > 1 {
        return Err(CrmError::invalid(
            "pipeline",
            "stages",
            "a pipeline may have at most one lost stage",
        ));
    }
    // The won and lost columns are the last two: a board whose outcome sits in the middle reads
    // as though the deal is still in play.
    for (index, (_, kind, _)) in validated.iter().enumerate() {
        let last_two = index + 2 >= validated.len();
        if (kind == "won" || kind == "lost") && !last_two {
            return Err(CrmError::invalid(
                "pipeline",
                "stages",
                "the won and lost stages are the last two columns",
            ));
        }
    }
    Ok(validated)
}

// ---------------------------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------------------------

/// Push every deal-list filter, in the documented order.
pub fn push_deal_filters(
    builder: &mut QueryBuilder<'_, sqlx::Postgres>,
    scope: &Scope,
    query: &ListQuery,
) -> Result<()> {
    builder.push(" where d.organization_id = ").push_bind(scope.organization_id);

    if !query.shows_archived() {
        builder.push(" and d.archived_at is null");
    }
    push_visibility(builder, scope, "d.owner_user_id");

    if let Some(term) = query.search_term()? {
        let pattern = format!("%{term}%");
        builder
            .push(" and (lower(d.title) like ")
            .push_bind(pattern.clone())
            .push(" or lower(coalesce(co.name, '')) like ")
            .push_bind(pattern.clone())
            .push(" or lower(coalesce(c.first_name, '')) like ")
            .push_bind(pattern.clone())
            .push(" or lower(coalesce(c.last_name, '')) like ")
            .push_bind(pattern)
            .push(")");
    }

    if let Some(pipeline_id) = query.pipeline_id {
        builder.push(" and d.pipeline_id = ").push_bind(pipeline_id);
    }
    if let Some(company_id) = query.company_id {
        builder.push(" and d.company_id = ").push_bind(company_id);
    }
    if let Some(contact_id) = query.contact_id {
        builder.push(" and d.contact_id = ").push_bind(contact_id);
    }
    if let Some(owner) = clean(query.owner.clone()) {
        if owner == "me" {
            builder.push(" and d.owner_user_id = ").push_bind(scope.user_id);
        } else if owner == "unassigned" {
            builder.push(" and d.owner_user_id is null");
        } else {
            let id = Uuid::parse_str(&owner).map_err(|_| {
                CrmError::InvalidQuery("owner is `me`, `unassigned` or a user identifier".to_owned())
            })?;
            builder.push(" and d.owner_user_id = ").push_bind(id);
        }
    }
    push_close_range(builder, query)?;
    Ok(())
}

/// Push the expected-close range, refusing a range that starts after it ends.
fn push_close_range(
    builder: &mut QueryBuilder<'_, sqlx::Postgres>,
    query: &ListQuery,
) -> Result<()> {
    if let (Some(from), Some(to)) = (query.created_from, query.created_to)
        && from > to
    {
        return Err(CrmError::InvalidQuery(
            "the close range starts after it ends".to_owned(),
        ));
    }
    if let Some(from) = query.created_from {
        builder
            .push(" and d.expected_close_on >= ")
            .push_bind(from);
    }
    if let Some(to) = query.created_to {
        // Inclusive of the last day.
        builder
            .push(" and d.expected_close_on < ")
            .push_bind(to + time::Duration::days(1));
    }
    Ok(())
}

/// The visibility clause, shared with the contact list's rule: a narrowed caller sees their own
/// records and the unassigned ones, because an unowned record belongs to nobody.
fn push_visibility<'a>(
    builder: &mut QueryBuilder<'a, sqlx::Postgres>,
    scope: &Scope,
    column: &str,
) {
    use crate::model::Visibility;
    match scope.visibility {
        Visibility::Own => {
            builder
                .push(" and (")
                .push(column)
                .push(" = ")
                .push_bind(scope.user_id)
                .push(" or ")
                .push(column)
                .push(" is null)");
        }
        Visibility::Team => {
            let mut ids = scope.team_user_ids.clone();
            if !ids.contains(&scope.user_id) {
                ids.push(scope.user_id);
            }
            builder
                .push(" and (")
                .push(column)
                .push(" = any(")
                .push_bind(ids)
                .push(") or ")
                .push(column)
                .push(" is null)");
        }
        Visibility::All => {}
    }
}

// ---------------------------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------------------------

/// One page of deals.
pub async fn list_deals(
    pool: &PgPool,
    scope: &Scope,
    query: &ListQuery,
) -> Result<Page<Deal>> {
    let (sort, desc) = query.resolve_sort("deals", "updated_at")?;
    let limit = query.page_size();
    let at = now();

    let mut builder: QueryBuilder<'_, sqlx::Postgres> = QueryBuilder::new(DEAL_SELECT);
    push_deal_filters(&mut builder, scope, query)?;

    // Keyset paging with the same shape the contact list uses: the sort key and the id, with
    // the cursor subquery filtered by the id, so two rows sharing a timestamp cannot bounce a
    // cursor forever.
    if let Some(cursor_id) = query.cursor_id()? {
        builder
            .push(" and (")
            .push(sort)
            .push(" , d.id) ")
            .push(if desc { "<" } else { ">" })
            .push(" (select ")
            .push(sort)
            .push(" , d.id from crm_deals d where d.id = ")
            .push_bind(cursor_id)
            .push(")");
    }

    builder
        .push(" order by ")
        .push(sort)
        .push(if desc { " desc, " } else { " asc, " })
        .push("d.id ")
        .push(if desc { "desc" } else { "asc" })
        .push(" limit ")
        .push_bind(limit + 1);

    let mut rows: Vec<DealRow> = builder.build_query_as().fetch_all(pool).await?;
    rows.truncate(limit as usize);
    let deals: Vec<Deal> = rows.into_iter().map(|row| row.into_deal(at)).collect();
    let cursor = next_cursor(&deals, |deal| deal.id);

    let total = count_deals(pool, scope, query).await?;
    Ok(Page::new(deals, cursor, total))
}

/// How many deals a filter matches.
pub async fn count_deals(pool: &PgPool, scope: &Scope, query: &ListQuery) -> Result<i64> {
    let mut builder: QueryBuilder<'_, sqlx::Postgres> = QueryBuilder::new(
        "select count(*) from crm_deals d \
         left join crm_companies co on co.id = d.company_id \
         left join crm_contacts c on c.id = d.contact_id",
    );
    push_deal_filters(&mut builder, scope, query)?;
    let (total,): (i64,) = builder.build_query_as().fetch_one(pool).await?;
    Ok(total)
}

/// One deal, or `NotFound` when it is not in the caller's scope.
pub async fn get_deal(pool: &PgPool, scope: &Scope, deal_id: Uuid) -> Result<Deal> {
    let mut builder: QueryBuilder<'_, sqlx::Postgres> = QueryBuilder::new(DEAL_SELECT);
    push_deal_filters(&mut builder, scope, &ListQuery::default())?;
    builder.push(" and d.id = ").push_bind(deal_id);

    let rows: Vec<DealRow> = builder.build_query_as().fetch_all(pool).await?;
    rows.into_iter()
        .next()
        .map(|row| row.into_deal(now()))
        .ok_or(CrmError::NotFound("deal"))
}

/// Every pipeline of the organization, each with its stages in board order.
pub async fn list_pipelines(pool: &PgPool, organization_id: Uuid) -> Result<Vec<Pipeline>> {
    #[derive(sqlx::FromRow)]
    struct PipelineRow {
        id: Uuid,
        organization_id: Uuid,
        name: String,
        is_default: bool,
        created_at: OffsetDateTime,
    }

    let mut pipelines: Vec<Pipeline> = sqlx::query_as::<_, PipelineRow>(
        "select id, organization_id, name, is_default, created_at \
         from crm_pipelines where organization_id = $1 order by is_default desc, lower(name)",
    )
    .bind(organization_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| Pipeline {
        id: row.id,
        organization_id: row.organization_id,
        name: row.name,
        is_default: row.is_default,
        stages: Vec::new(),
        created_at: row.created_at,
    })
    .collect();

    for pipeline in &mut pipelines {
        pipeline.stages = list_stages(pool, pipeline.id).await?;
    }
    Ok(pipelines)
}

/// The stages of one pipeline, in board order.
///
/// Generic over the executor, not because it is reusable but because the stage **move** has to
/// read them inside its transaction: taking a `&PgPool` there would read the stages outside the
/// transaction, so a stage deleted between the drag and the write would be a card moved into a
/// column that no longer exists.
pub async fn list_stages<'e, E>(executor: E, pipeline_id: Uuid) -> Result<Vec<PipelineStage>>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    #[derive(sqlx::FromRow)]
    struct StageRow {
        id: Uuid,
        organization_id: Uuid,
        pipeline_id: Uuid,
        name: String,
        kind: String,
        position: i32,
        probability: i32,
    }

    Ok(sqlx::query_as::<_, StageRow>(
        "select id, organization_id, pipeline_id, name, kind, position, probability \
         from crm_pipeline_stages where pipeline_id = $1 order by position",
    )
    .bind(pipeline_id)
    .fetch_all(executor)
    .await?
    .into_iter()
    .map(|row| PipelineStage {
        id: row.id,
        organization_id: row.organization_id,
        pipeline_id: row.pipeline_id,
        name: row.name,
        kind: row.kind,
        position: row.position,
        probability: row.probability,
    })
    .collect())
}

/// One pipeline with its stages, or `NotFound` when it is not in this organization.
pub async fn get_pipeline(pool: &PgPool, organization_id: Uuid, pipeline_id: Uuid) -> Result<Pipeline> {
    list_pipelines(pool, organization_id)
        .await?
        .into_iter()
        .find(|pipeline| pipeline.id == pipeline_id)
        .ok_or(CrmError::NotFound("pipeline"))
}

/// The organization's default pipeline, or the first one that exists.
pub async fn default_pipeline(pool: &PgPool, organization_id: Uuid) -> Result<Pipeline> {
    // A one-column query reads as a typed row, not as `PgRow::get(0)`: an index into an
    // untyped row can be broken by a column reorder without the compiler noticing.
    #[derive(sqlx::FromRow)]
    struct PipelineId {
        id: Uuid,
    }

    let mut builder: QueryBuilder<'_, sqlx::Postgres> =
        QueryBuilder::new("select id from crm_pipelines where organization_id = ");
    builder
        .push_bind(organization_id)
        .push(" order by is_default desc, created_at limit 1");

    let found: Option<PipelineId> = builder.build_query_as().fetch_optional(pool).await?;
    let id = found.ok_or(CrmError::NotFound("pipeline"))?.id;
    get_pipeline(pool, organization_id, id).await
}

/// The per-stage totals of a board, in board order.
///
/// **One statement, one expression.** The count, the sum and the weighted sum are computed
/// together with the same `left join … on d.stage_id = s.id` that decides which deals count, so
/// a column header and the cards in that column can never come from different questions. The
/// empty stages are kept (`deal_count = 0`) because a column with no cards is still a column.
pub async fn stage_totals(pool: &PgPool, scope: &Scope, pipeline_id: Uuid) -> Result<Vec<StageTotals>> {
    #[derive(sqlx::FromRow)]
    struct TotalRow {
        stage_id: Uuid,
        name: String,
        kind: String,
        position: i32,
        probability: i32,
        deal_count: i64,
        total: String,
        weighted_total: String,
    }

    // The whole statement is a `QueryBuilder`, binds and placeholders included: mixing a
    // `format!`'d `$1` with a pushed bind silently shifts every placeholder after the first
    // one, which reads as a wrong total rather than as a broken statement.
    //
    // The visibility clause is the *same* one the list uses, applied inside the `left join`'s
    // `on` clause: a caller narrowed to `own` sees their own cards, and a column's total
    // therefore counts only what that caller may see. Putting it in `where` instead would drop
    // the empty columns entirely — a narrowed caller would lose the board's shape.
    let mut builder: QueryBuilder<'_, sqlx::Postgres> = QueryBuilder::new(
        "select s.id as stage_id, s.name, s.kind, s.position, s.probability, \
           count(d.id) as deal_count, \
           coalesce(sum(d.amount), 0)::text as total, \
           coalesce(sum(d.amount * coalesce(d.probability, s.probability) / 100), 0)::text \
             as weighted_total \
         from crm_pipeline_stages s \
         left join crm_deals d on d.stage_id = s.id \
              and d.archived_at is null \
              and d.organization_id = ",
    );
    builder.push_bind(scope.organization_id);
    builder.push(" ");
    push_visibility(&mut builder, scope, "d.owner_user_id");
    builder
        .push(" where s.pipeline_id = ")
        .push_bind(pipeline_id)
        .push(" group by s.id, s.name, s.kind, s.position, s.probability order by s.position");

    let rows: Vec<TotalRow> = builder.build_query_as().fetch_all(pool).await?;

    Ok(rows
        .into_iter()
        .map(|row| StageTotals {
            stage_id: row.stage_id,
            name: row.name,
            kind: row.kind,
            position: row.position,
            probability: row.probability,
            deal_count: row.deal_count,
            total: row.total,
            weighted_total: row.weighted_total,
        })
        .collect())
}

/// The whole board in one call: the pipeline, its columns' totals and the cards.
pub async fn board_payload(pool: &PgPool, scope: &Scope, pipeline_id: Uuid) -> Result<Board> {
    let pipeline = get_pipeline(pool, scope.organization_id, pipeline_id).await?;
    let columns = stage_totals(pool, scope, pipeline_id).await?;

    let mut query = ListQuery::paginated(crate::query::MAX_PER_PAGE);
    query.pipeline_id = Some(pipeline_id);
    let deals = list_deals(pool, scope, &query).await?.items;

    // The forecast is the sum of the **open** columns only: a won deal is revenue, not
    // something still to win, and a lost one is nothing at all. Summing the columns the board
    // already shows means the footer can never disagree with the headers.
    let (open_total, weighted_forecast) = columns
        .iter()
        .filter(|column| column.kind == "open")
        .fold((String::new(), String::new()), |(total, weighted), column| {
            (add_money(&total, &column.total), add_money(&weighted, &column.weighted_total))
        });

    Ok(Board {
        pipeline,
        columns,
        deals,
        open_total,
        weighted_forecast,
    })
}

/// Add two money strings without going through a float.
///
/// The values are `numeric(14,2)` sums, and a deal board that shows `0.30000000000000004` in a
/// column header is a board nobody trusts. The arithmetic is done in integer minor units.
#[must_use]
pub fn add_money(left: &str, right: &str) -> String {
    let to_minor = |value: &str| -> i64 {
        let value = value.trim();
        let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
        let fraction: String = fraction.chars().filter(char::is_ascii_digit).collect();
        let fraction = format!("{fraction:0<2}");
        whole
            .parse::<i64>()
            .unwrap_or(0)
            .saturating_mul(100)
            .saturating_add(fraction.parse::<i64>().unwrap_or(0))
    };
    let sum = to_minor(left).saturating_add(to_minor(right));
    format!("{}.{:02}", sum / 100, sum.unsigned_abs() % 100)
}

// ---------------------------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------------------------

/// Create a deal on the given (or the default) pipeline and stage.
pub async fn create_deal(
    pool: &PgPool,
    organization_id: Uuid,
    owner_fallback: Option<Uuid>,
    changes: &DealChanges,
) -> Result<Deal> {
    let normalised = validate_deal(changes)?;

    let (pipeline_id, stage_id, stage_kind, stage_probability) =
        resolve_target_stage(pool, organization_id, changes).await?;

    // A deal created straight into a lost stage has to say why here too, or the schema's
    // `before insert` trigger would refuse it with a `check_violation` the form cannot show.
    let lost_reason = if stage_kind == "lost" {
        Some(
            validate_lost_reason(changes.lost_reason.as_deref())?.ok_or_else(|| {
                CrmError::InvalidStageChange(
                    "a deal cannot start in a lost stage without a reason".to_owned(),
                )
            })?,
        )
    } else {
        None
    };

    let probability = normalised
        .probability
        .or(if stage_kind == "won" { Some(100) } else { Some(stage_probability) });
    let id = Uuid::new_v4();

    sqlx::query(
        "insert into crm_deals (id, organization_id, pipeline_id, stage_id, title, company_id, \
         contact_id, owner_user_id, amount, currency, probability, expected_close_on, source, \
         lost_reason) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9::numeric, $10, $11, $12, $13, $14)",
    )
    .bind(id)
    .bind(organization_id)
    .bind(pipeline_id)
    .bind(stage_id)
    .bind(&normalised.title)
    .bind(changes.company_id)
    .bind(changes.contact_id)
    .bind(changes.owner_user_id.or(owner_fallback))
    .bind(&normalised.amount)
    .bind(&normalised.currency)
    .bind(probability)
    .bind(normalised.expected_close_on)
    .bind(&normalised.source)
    .bind(&lost_reason)
    .execute(pool)
    .await?;

    read_deal(pool, organization_id, id).await
}

/// The (pipeline, stage, kind, probability) a new deal lands on.
async fn resolve_target_stage(
    pool: &PgPool,
    organization_id: Uuid,
    changes: &DealChanges,
) -> Result<(Uuid, Uuid, String, i32)> {
    let pipeline_id = match changes.pipeline_id {
        Some(id) => {
            get_pipeline(pool, organization_id, id).await?;
            id
        }
        None => default_pipeline(pool, organization_id).await?.id,
    };

    let stages = list_stages(pool, pipeline_id).await?;

    let stage = match changes.stage_id {
        Some(stage_id) => stages
            .into_iter()
            .find(|stage| stage.id == stage_id)
            .ok_or_else(|| {
                CrmError::InvalidStageChange(
                    "the stage does not belong to this pipeline".to_owned(),
                )
            })?,
        None => {
            // The first **open** stage: a new deal that appeared in a lost column would need a
            // reason nobody has given yet. Written without a closure over `.await` because an
            // async block does not fit in `or_else`.
            match stages.iter().find(|stage| stage.kind == "open") {
                Some(found) => found.clone(),
                None => stages
                    .first()
                    .cloned()
                    .ok_or(CrmError::NotFound("stage"))?,
            }
        }
    };

    Ok((pipeline_id, stage.id, stage.kind, stage.probability))
}

/// Apply a partial update to a deal's own fields.
pub async fn patch_deal(
    pool: &PgPool,
    scope: &Scope,
    deal_id: Uuid,
    patch: &DealPatch,
) -> Result<Deal> {
    // Read first, so a patch is validated against what exists and an out-of-scope id is a 404
    // before a statement is ever written.
    let current = get_deal(pool, scope, deal_id).await?;

    let mut builder: QueryBuilder<'_, sqlx::Postgres> = QueryBuilder::new("update crm_deals set ");
    let mut touched = 0usize;

    macro_rules! set {
        ($column:expr, $value:expr) => {{
            if touched > 0 {
                builder.push(", ");
            }
            builder.push($column).push(" = ").push_bind($value);
            touched += 1;
        }};
    }

    // Same as `set!`, but the column is numeric and the value arrives as a normalised money
    // string. Postgres will not coerce a text parameter into `numeric` on its own, and a bare
    // `amount = $n` answers "column amount is of type numeric but expression is of type text" —
    // so the cast belongs in the statement, and money stays `numeric` rather than becoming a float.
    macro_rules! set_numeric {
        ($column:expr, $value:expr) => {{
            if touched > 0 {
                builder.push(", ");
            }
            builder
                .push($column)
                .push(" = ")
                .push_bind($value)
                .push("::numeric");
            touched += 1;
        }};
    }

    if let Some(title) = patch.title.as_deref() {
        let title = title.trim();
        if title.is_empty() {
            return Err(CrmError::invalid("deal", "title", "a deal needs a title"));
        }
        if title.chars().count() > MAX_TITLE_LENGTH {
            return Err(CrmError::invalid(
                "deal",
                "title",
                format!("a deal title is at most {MAX_TITLE_LENGTH} characters"),
            ));
        }
        set!("title", title.to_owned());
    }
    if patch.company_id.is_some() {
        set!("company_id", patch.company_id);
    }
    if patch.contact_id.is_some() {
        set!("contact_id", patch.contact_id);
    }
    if let Some(owner) = patch.owner_user_id {
        set!("owner_user_id", owner);
    }
    if let Some(amount) = patch.amount.as_deref() {
        set_numeric!("amount", validate_amount(Some(amount))?);
    }
    if let Some(currency) = patch.currency.as_deref() {
        set!("currency", validate_currency(Some(currency))?);
    }
    if let Some(probability) = patch.probability {
        set!("probability", validate_probability(Some(probability), "deal")?);
    }
    if patch.expected_close_on.is_some() {
        set!("expected_close_on", patch.expected_close_on);
    }
    if let Some(source) = patch.source.clone() {
        let source = clean(Some(source));
        if source
            .as_ref()
            .is_some_and(|text| text.chars().count() > MAX_SOURCE_LENGTH)
        {
            return Err(CrmError::invalid(
                "deal",
                "source",
                format!("a source is at most {MAX_SOURCE_LENGTH} characters"),
            ));
        }
        set!("source", source);
    }

    if touched == 0 {
        return Ok(current);
    }

    builder
        .push(", updated_at = now() where id = ")
        .push_bind(deal_id)
        .push(" and organization_id = ")
        .push_bind(scope.organization_id);

    let outcome: PgQueryResult = builder.build().execute(pool).await?;
    if outcome.rows_affected() == 0 {
        return Err(CrmError::NotFound("deal"));
    }

    read_deal(pool, scope.organization_id, deal_id).await
}

/// Move a deal to another stage: the drag, and the keyboard's `ctrl + ←/→`.
///
/// Transactional, and in this order: the deal is read and the target stage is resolved *inside*
/// the transaction, so a stage that is deleted between the drag and the write is a refusal
/// rather than a card in a column of a pipeline that no longer has one.
pub async fn move_deal_stage(
    pool: &PgPool,
    scope: &Scope,
    deal_id: Uuid,
    move_to: &StageMove,
    today: Date,
) -> Result<Deal> {
    let mut tx = pool.begin().await?;

    let mut finder: QueryBuilder<'_, sqlx::Postgres> = QueryBuilder::new(DEAL_SELECT);
    push_deal_filters(&mut finder, scope, &ListQuery::default())?;
    finder.push(" and d.id = ").push_bind(deal_id);

    let rows: Vec<DealRow> = finder.build_query_as().fetch_all(&mut *tx).await?;
    let current = rows
        .into_iter()
        .next()
        .map(|row| row.into_deal(now()))
        .ok_or(CrmError::NotFound("deal"))?;

    // The target stage must be a stage of **this deal's** pipeline.
    let stage = list_stages(&mut *tx, current.pipeline_id)
        .await?
        .into_iter()
        .find(|stage| stage.id == move_to.stage_id)
        .ok_or_else(|| {
            CrmError::InvalidStageChange("the stage does not belong to this pipeline".to_owned())
        })?;

    let resolved = resolve_move(move_to, &stage.kind, today)?;

    let mut builder: QueryBuilder<'_, sqlx::Postgres> = QueryBuilder::new("update crm_deals set ");
    builder.push("stage_id = ").push_bind(stage.id);
    builder.push(", lost_reason = ").push_bind(resolved.lost_reason.clone());
    if let Some(close_on) = resolved.close_on {
        builder.push(", expected_close_on = ").push_bind(close_on);
        // A won deal's probability is 100 whatever the stage's default says; otherwise a won
        // column entered at 80% would credit four fifths of a deal that is already, by
        // definition, won — and the board's "won this quarter" total would not be revenue.
        builder.push(", probability = 100");
    }
    builder
        .push(", stage_changed_at = now(), updated_at = now() where id = ")
        .push_bind(deal_id)
        .push(" and organization_id = ")
        .push_bind(scope.organization_id);

    let outcome: PgQueryResult = builder.build().execute(&mut *tx).await?;
    if outcome.rows_affected() == 0 {
        return Err(CrmError::NotFound("deal"));
    }
    tx.commit().await?;

    read_deal(pool, scope.organization_id, deal_id).await
}

/// Archive a deal: the soft removal the board hides and the history keeps.
pub async fn archive_deal(pool: &PgPool, scope: &Scope, deal_id: Uuid) -> Result<Deal> {
    let result = sqlx::query(
        "update crm_deals set archived_at = now(), updated_at = now() \
         where id = $1 and organization_id = $2 and archived_at is null",
    )
    .bind(deal_id)
    .bind(scope.organization_id)
    .execute(pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(CrmError::NotFound("deal"));
    }

    read_deal(pool, scope.organization_id, deal_id).await
}

/// Replace a pipeline's stages with the ordered set the editor sends.
///
/// The `position` column is unique per pipeline, so the rows are moved out of the way first
/// inside the transaction: without the two-step, reordering a pipeline in place would hit the
/// unique index on the second row and answer a `500` for a drag that worked perfectly well in
/// the user's head.
pub async fn replace_stages(
    pool: &PgPool,
    organization_id: Uuid,
    pipeline_id: Uuid,
    set: &StageSet,
) -> Result<Vec<PipelineStage>> {
    let validated = validate_stage_set(set)?;
    let existing = get_pipeline(pool, organization_id, pipeline_id).await?;

    // Stage ids the editor kept, in the order it sent them. A stage whose id is not in the set
    // and that still holds deals is refused rather than deleted: `on delete restrict` would
    // answer a `500` naming a foreign key, and a board cannot ask its person to guess.
    let kept: Vec<Uuid> = set
        .stages
        .iter()
        .enumerate()
        .filter_map(|(index, _)| existing.stages.get(index).map(|stage| stage.id))
        .collect();
    let dropped: Vec<Uuid> = existing
        .stages
        .iter()
        .filter(|stage| !kept.contains(&stage.id))
        .map(|stage| stage.id)
        .collect();
    if !dropped.is_empty() {
        let holders: i64 = sqlx::query_scalar(
            "select count(*) from crm_deals where stage_id = any($1) and archived_at is null",
        )
        .bind(&dropped)
        .fetch_one(pool)
        .await?;
        if holders > 0 {
            return Err(CrmError::InvalidStageChange(format!(
                "{} stage(s) still hold {holders} deal(s); move them before removing the column",
                dropped.len()
            )));
        }
    }

    let mut tx = pool.begin().await?;

    // Free the positions first: the unique index is per pipeline, so a reorder in place would
    // collide on the very first row that keeps its number and moves down.
    sqlx::query(
        "update crm_pipeline_stages set position = position + 10000 \
         where pipeline_id = $1",
    )
    .bind(pipeline_id)
    .execute(&mut *tx)
    .await?;

    for (index, (name, kind, probability)) in validated.iter().enumerate() {
        let id = kept.get(index).copied();
        let position = i32::try_from(index).unwrap_or(i32::MAX);
        if let Some(stage_id) = id {
            sqlx::query(
                "update crm_pipeline_stages \
                 set name = $1, kind = $2, position = $3, probability = $4, updated_at = now() \
                 where id = $5 and pipeline_id = $6",
            )
            .bind(name)
            .bind(kind)
            .bind(position)
            .bind(*probability)
            .bind(stage_id)
            .bind(pipeline_id)
            .execute(&mut *tx)
            .await?;
        } else {
            sqlx::query(
                "insert into crm_pipeline_stages \
                 (organization_id, pipeline_id, name, kind, position, probability) \
                 values ($1, $2, $3, $4, $5, $6)",
            )
            .bind(organization_id)
            .bind(pipeline_id)
            .bind(name)
            .bind(kind)
            .bind(position)
            .bind(*probability)
            .execute(&mut *tx)
            .await?;
        }
    }

    if !dropped.is_empty() {
        sqlx::query("delete from crm_pipeline_stages where id = any($1)")
            .bind(&dropped)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;

    list_stages(pool, pipeline_id).await
}

/// Read one deal by organization, without a visibility filter (the create/write path).
async fn read_deal(pool: &PgPool, organization_id: Uuid, deal_id: Uuid) -> Result<Deal> {
    let rows: Vec<DealRow> = sqlx::query_as(&format!(
        "{DEAL_SELECT} where d.organization_id = $1 and d.id = $2"
    ))
    .bind(organization_id)
    .bind(deal_id)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .next()
        .map(|row| row.into_deal(now()))
        .ok_or(CrmError::NotFound("deal"))
}

/// `true` when a contact or company id may hang a deal off, checked against the organization.
///
/// A deal pointing at another tenant's company is not a privacy leak in one direction (the join
/// would find nothing) but it is a referential oddity that shows up as a card with no company on
/// somebody's board, so the form refuses it with the field named.
pub async fn is_same_organization(pool: &PgPool, organization_id: Uuid, table: &str, id: Uuid) -> Result<bool> {
    let sql = match table {
        "company" => "select 1 from crm_companies where id = $1 and organization_id = $2 and archived_at is null",
        "contact" => "select 1 from crm_contacts where id = $1 and organization_id = $2 and archived_at is null",
        _ => return Ok(false),
    };
    let found: Option<i32> = sqlx::query_scalar(sql)
        .bind(id)
        .bind(organization_id)
        .fetch_optional(pool)
        .await?;
    Ok(found.is_some())
}

/// Re-exported so the route layer validates the same shapes the form was shown, rather than
/// a second copy that can drift.
pub use self::{validate_amount as amount_value, validate_currency as currency_code};

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    #[test]
    fn the_weighted_forecast_is_the_documented_expression() {
        // `amount × probability / 100`, to the cent — the number the column header shows.
        assert_eq!(weighted_amount("1000.00", 25), "250.00");
        assert_eq!(weighted_amount("1000.00", 0), "0.00");
        assert_eq!(weighted_amount("1000.00", 100), "1000.00");
        assert_eq!(weighted_amount("0.00", 80), "0.00");
        // `33.33 × 33 % = 10.9989` rounds **up** to 11.00. Written down because the tempting
        // expectation is 10.99: truncating instead of rounding would make the Rust preview
        // disagree with the SQL `numeric`, which rounds.
        assert_eq!(weighted_amount("33.33", 33), "11.00");
    }

    #[test]
    fn money_adds_in_minor_units_not_floats() {
        // The float version of this sum prints `0.30000000000000004`; a board header that shows
        // that is a board nobody trusts.
        assert_eq!(add_money("0.10", "0.20"), "0.30");
        assert_eq!(add_money("1000", "0.50"), "1000.50");
        assert_eq!(add_money("0", "0"), "0.00");
        assert_eq!(add_money("12500.25", "999.75"), "13500.00");
    }

    #[test]
    fn a_deal_needs_a_title() {
        let error = validate_deal(&DealChanges {
            title: "   ".to_owned(),
            ..DealChanges::default()
        })
        .expect_err("a blank title must be refused");
        assert!(error.to_string().contains("deal.title"), "{error}");

        let too_long = "x".repeat(MAX_TITLE_LENGTH + 1);
        let error = validate_deal(&DealChanges {
            title: too_long,
            ..DealChanges::default()
        })
        .expect_err("an over-long title must be refused");
        assert!(error.to_string().contains("200"), "{error}");
    }

    #[test]
    fn a_value_is_read_whatever_the_separator_the_locale_uses() {
        assert_eq!(validate_amount(Some("12500")).unwrap(), "12500.00");
        assert_eq!(validate_amount(Some("12500.5")).unwrap(), "12500.50");
        assert_eq!(validate_amount(Some("1,250.75")).unwrap(), "1250.75");
        // A Turkish keyboard sends `1.250,75`: the last separator is the decimal mark.
        assert_eq!(validate_amount(Some("1.250,75")).unwrap(), "1250.75");
        assert_eq!(validate_amount(Some("12,5")).unwrap(), "12.50");
        assert_eq!(validate_amount(None).unwrap(), "0.00");
        assert_eq!(validate_amount(Some("  ")).unwrap(), "0.00");
    }

    #[test]
    fn a_value_that_is_not_a_number_is_refused_with_the_field() {
        for bad in ["abc", "-5", "1.2.3.4", "1e400"] {
            let error = validate_amount(Some(bad)).expect_err("must be refused");
            assert!(error.to_string().contains("deal.amount"), "{bad}: {error}");
        }
    }

    #[test]
    fn a_currency_is_three_upper_case_letters_the_form_offers() {
        assert_eq!(validate_currency(Some("eur")).unwrap(), "EUR");
        assert_eq!(validate_currency(None).unwrap(), "USD");
        for bad in ["EURO", "E", "12"] {
            assert!(validate_currency(Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_probability_outside_the_range_is_refused() {
        assert_eq!(validate_probability(Some(0), "deal").unwrap(), Some(0));
        assert_eq!(validate_probability(Some(100), "deal").unwrap(), Some(100));
        for bad in [-1, 101, 1000] {
            let error =
                validate_probability(Some(bad), "deal").expect_err("must be refused");
            assert!(error.to_string().contains("between 0 and 100"), "{error}");
        }
    }

    #[test]
    fn moving_into_the_lost_column_requires_a_reason() {
        let today = date!(2026 - 09 - 27);
        let error = resolve_move(
            &StageMove { stage_id: Uuid::nil(), ..StageMove::default() },
            "lost",
            today,
        )
        .expect_err("a loss without a reason must be refused");
        assert!(error.to_string().contains("without a reason"), "{error}");

        let resolved = resolve_move(
            &StageMove {
                stage_id: Uuid::nil(),
                lost_reason: Some("  chose a competitor  ".to_owned()),
                ..StageMove::default()
            },
            "lost",
            today,
        )
        .expect("a reason is enough");
        assert_eq!(resolved.lost_reason.as_deref(), Some("chose a competitor"));
    }

    #[test]
    fn moving_into_the_won_column_records_the_close_date() {
        let today = date!(2026 - 09 - 27);
        let confirmed = date!(2026 - 09 - 30);

        // Without a confirmation the deal is won today.
        let resolved = resolve_move(
            &StageMove { stage_id: Uuid::nil(), ..StageMove::default() },
            "won",
            today,
        )
        .expect("a win needs no reason");
        assert_eq!(resolved.close_on, Some(today));
        // Leaving the lost column forgets the old reason.
        assert_eq!(resolved.lost_reason, None);

        // A confirmed date wins over "today".
        let resolved = resolve_move(
            &StageMove {
                stage_id: Uuid::nil(),
                close_on: Some(confirmed),
                ..StageMove::default()
            },
            "won",
            today,
        )
        .expect("a confirmed date is accepted");
        assert_eq!(resolved.close_on, Some(confirmed));
    }

    #[test]
    fn an_open_stage_needs_nothing_but_the_move_itself() {
        let today = date!(2026 - 09 - 27);
        let resolved = resolve_move(
            &StageMove { stage_id: Uuid::nil(), ..StageMove::default() },
            "open",
            today,
        )
        .expect("an open move is always allowed");
        assert_eq!(resolved, ResolvedMove { lost_reason: None, close_on: None });
    }

    #[test]
    fn a_lost_reason_is_bounded() {
        let long = "x".repeat(MAX_LOST_REASON_LENGTH + 1);
        let error = validate_lost_reason(Some(&long)).expect_err("must be refused");
        assert!(error.to_string().contains("lost_reason"), "{error}");
        assert_eq!(validate_lost_reason(Some("  ")).unwrap(), None);
    }

    #[test]
    fn a_pipeline_needs_an_open_stage_and_one_outcome_at_most() {
        let entry = |name: &str, kind: &str, probability: i32| StageChanges {
            name: name.to_owned(),
            kind: Some(kind.to_owned()),
            probability: Some(probability),
        };

        let set = StageSet {
            stages: vec![
                entry("New", "open", 10),
                entry("Negotiation", "open", 80),
                entry("Won", "won", 100),
                entry("Lost", "lost", 0),
            ],
        };
        assert!(validate_stage_set(&set).is_ok());

        // Two winning columns would double-count the forecast.
        let two_won = StageSet {
            stages: vec![
                entry("New", "open", 10),
                entry("Won", "won", 100),
                entry("Renewal", "won", 100),
            ],
        };
        let error = validate_stage_set(&two_won).expect_err("two won columns");
        assert!(error.to_string().contains("at most one won"), "{error}");

        // No open stage means nowhere for a new deal to land.
        let no_open = StageSet {
            stages: vec![entry("Won", "won", 100)],
        };
        let error = validate_stage_set(&no_open).expect_err("no open stage");
        assert!(error.to_string().contains("at least one open"), "{error}");

        // The outcome columns cannot sit in the middle of the board.
        let middle = StageSet {
            stages: vec![
                entry("Won", "won", 100),
                entry("Negotiation", "open", 80),
                entry("Lost", "lost", 0),
            ],
        };
        let error = validate_stage_set(&middle).expect_err("outcome in the middle");
        assert!(error.to_string().contains("last two"), "{error}");
    }

    #[test]
    fn a_won_column_is_always_a_hundred_percent() {
        let entry = StageChanges {
            name: "Won".to_owned(),
            kind: Some("won".to_owned()),
            // A person typing 80 into the won column would otherwise credit 80% of a deal
            // that is already, by definition, won.
            probability: Some(80),
        };
        assert_eq!(validate_stage(&entry).unwrap().2, 100);
    }

    #[test]
    fn an_empty_pipeline_is_refused_before_the_database_sees_it() {
        let error = validate_stage_set(&StageSet::default()).expect_err("empty");
        assert!(error.to_string().contains("at least one stage"), "{error}");

        // A pipeline of exactly one open stage is valid — the smallest board there is.
        let single = validate_stage_set(&StageSet {
            stages: vec![StageChanges {
                name: "New".to_owned(),
                kind: None,
                probability: None,
            }],
        })
        .expect("a single open stage is allowed");
        assert_eq!(single, vec![("New".to_owned(), "open".to_owned(), 0)]);
    }

    #[test]
    fn a_card_goes_stale_after_thirty_days() {
        let today = date!(2026 - 09 - 27);
        let recent = OffsetDateTime::from_unix_timestamp(
            (today.midnight().assume_utc() - time::Duration::days(5)).unix_timestamp(),
        )
        .unwrap();
        let old = OffsetDateTime::from_unix_timestamp(
            (today.midnight().assume_utc() - time::Duration::days(45)).unix_timestamp(),
        )
        .unwrap();
        let at = today.midnight().assume_utc();

        assert!(!is_stale(recent, at));
        assert!(is_stale(old, at));
        assert_eq!(days_in_stage(old, at), 45);
        // A stage changed "in the future" (clock skew between the API and the database) is not
        // a negative age on the card.
        assert_eq!(days_in_stage(at + time::Duration::days(2), at), 0);
    }

    #[test]
    fn the_stage_kinds_are_the_three_documented_ones() {
        assert!(is_stage_kind("open"));
        assert!(is_stage_kind(" won "));
        for bad in ["", "closed", "Open!", "OPENED"] {
            assert!(!is_stage_kind(bad), "{bad}");
        }
    }
}
