//! The metric catalogue in the database, seeded from the registry (REQ-126, slice 2).
//!
//! The registry in [`crate::metrics`] is the declaration; this is its durable projection. The
//! split matters for three reasons the migration's comment spells out, and the one that bites first
//! in review is the simplest: **a family that has been declared but has never recorded a sample
//! and a family that has no samples right now are different states**, and only a `last_seen_at`
//! column tells them apart. An operator looking at a catalogue of twenty-one families needs to
//! know which of the eight empty ones are broken and which are simply not used by this instance's
//! configuration.
//!
//! ## Seeding is an upsert, never a truncate-and-insert
//!
//! A module registers its own families into the registry at boot, and its rows are in the same
//! table as the core's. Deleting the table on boot would therefore delete another package's
//! documentation. [`sync_from_registry`] upserts the families it knows about and leaves everything
//! else alone, which also means a family the *core* no longer declares keeps its row and its
//! `last_seen_at` — a visible, honest "this was emitted by a build that is no longer running"
//! rather than a row that vanishes and takes the evidence with it.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::TelemetryError;
use crate::metrics::{self, FamilySpec, MetricKind};

/// One catalogue row, in the panel's shape.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CatalogRow {
    /// The row's id.
    pub id: Uuid,
    /// The exposition name.
    pub name: String,
    /// `counter`, `gauge` or `histogram`.
    pub kind: String,
    /// The unit.
    pub unit: String,
    /// The one-sentence description.
    pub description: String,
    /// The label names, positionally.
    pub labels: Vec<String>,
    /// `core`, `module` or `worker`.
    pub source: String,
    /// The series count measured at the last boot.
    pub cardinality_estimate: i32,
    /// The cap this family is held to.
    pub cardinality_budget: i32,
    /// Whether the cap is enforced for this family.
    pub budgeted: bool,
    /// When the family last recorded a sample.
    pub last_seen_at: Option<OffsetDateTime>,
    /// When the row was written.
    pub updated_at: OffsetDateTime,
}

/// The registry's view of a family, paired with its live state.
///
/// [`sync_from_registry`] takes this rather than the registry itself so a test can seed a family
/// the running process does not have — which is the only way to prove that an unknown family is
/// *left alone* instead of quietly deleted.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FamilyDeclaration {
    /// The exposition name.
    pub name: String,
    /// The kind, as the migration's check constraint spells it.
    pub kind: String,
    /// The unit.
    pub unit: String,
    /// The description, also used as the exposition's HELP line.
    pub description: String,
    /// The label names, positionally.
    pub labels: Vec<String>,
    /// Who is expected to record it.
    pub source: String,
    /// The cap.
    pub cardinality_budget: i32,
    /// Whether the cap is enforced.
    pub budgeted: bool,
    /// The series count right now, or `0` when the family has not been recorded.
    pub cardinality_estimate: i32,
}

impl FamilyDeclaration {
    /// A declaration from a [`FamilySpec`] and the live series count for it.
    #[must_use]
    pub fn from_spec(spec: &FamilySpec, series: usize) -> Self {
        Self {
            name: spec.name.to_owned(),
            kind: spec.kind.as_str().to_owned(),
            unit: spec.unit.to_owned(),
            description: spec.description.to_owned(),
            labels: spec
                .labels
                .iter()
                .map(|label| (*label).to_owned())
                .collect(),
            source: spec.source.to_owned(),
            cardinality_budget: i32::try_from(spec.max_series).unwrap_or(i32::MAX),
            budgeted: spec.bounded_labels,
            cardinality_estimate: i32::try_from(series).unwrap_or(i32::MAX),
        }
    }

    /// A declaration of a family the running process does not have, for a test or for a module
    /// that registered a family the core does not know.
    #[must_use]
    pub fn custom(name: &str, kind: MetricKind, labels: &[&str]) -> Self {
        Self {
            name: name.to_owned(),
            kind: kind.as_str().to_owned(),
            unit: "1".to_owned(),
            description: "A family registered by a module.".to_owned(),
            labels: labels.iter().map(|label| (*label).to_owned()).collect(),
            source: "module".to_owned(),
            cardinality_budget: 64,
            budgeted: true,
            cardinality_estimate: 0,
        }
    }
}

/// Upsert the registry's families into the catalogue.
///
/// Returns how many rows were written. A family already present keeps its identity — the `name` is
/// the unique key and the id is never regenerated, because a future settings screen that points at
/// a family by id must not have its reference broken by a restart.
pub async fn sync_from_registry(
    pool: &PgPool,
    declarations: &[FamilyDeclaration],
) -> Result<u64, TelemetryError> {
    let mut written = 0_u64;
    for declaration in declarations {
        let changed = sqlx::query(
            "insert into obs_metric_catalog \
                 (name, kind, unit, description, labels, source, cardinality_estimate, \
                  cardinality_budget, budgeted, updated_at) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, now()) \
             on conflict (name) do update set \
                 kind = excluded.kind, \
                 unit = excluded.unit, \
                 description = excluded.description, \
                 labels = excluded.labels, \
                 source = excluded.source, \
                 cardinality_estimate = excluded.cardinality_estimate, \
                 cardinality_budget = excluded.cardinality_budget, \
                 budgeted = excluded.budgeted, \
                 updated_at = now() \
             where obs_metric_catalog.kind        is distinct from excluded.kind \
                or obs_metric_catalog.unit        is distinct from excluded.unit \
                or obs_metric_catalog.description is distinct from excluded.description \
                or obs_metric_catalog.labels      is distinct from excluded.labels \
                or obs_metric_catalog.source      is distinct from excluded.source \
                or obs_metric_catalog.cardinality_estimate <> excluded.cardinality_estimate \
                or obs_metric_catalog.cardinality_budget   <> excluded.cardinality_budget \
                or obs_metric_catalog.budgeted    <> excluded.budgeted \
             returning id",
        )
        .bind(&declaration.name)
        .bind(&declaration.kind)
        .bind(&declaration.unit)
        .bind(&declaration.description)
        .bind(&declaration.labels)
        .bind(&declaration.source)
        .bind(declaration.cardinality_estimate)
        .bind(declaration.cardinality_budget)
        .bind(declaration.budgeted)
        .fetch_optional(pool)
        .await?;
        if changed.is_some() {
            written += 1;
        }
    }
    Ok(written)
}

/// Note that a family recorded a sample, so `last_seen_at` is meaningful.
///
/// Called on a slow cadence rather than per sample: a column written on every HTTP request is a
/// write the request pays for, and the panel's question is "is this family alive at all?", which a
/// once-a-minute answer serves exactly.
pub async fn mark_seen(pool: &PgPool, names: &[&str]) -> Result<u64, TelemetryError> {
    if names.is_empty() {
        return Ok(0);
    }
    // A one-minute floor on the write: the column is a liveness stamp, and stamping it on every
    // sample would make the observability system the most-written table in the platform.
    let result = sqlx::query(
        "update obs_metric_catalog set last_seen_at = now() \
         where name = any($1) \
           and (last_seen_at is null or last_seen_at < now() - interval '1 minute')",
    )
    .bind(names)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Every family, in the registry's declaration order, with the live series counts merged in.
///
/// The counts come from the registry rather than from the table's `cardinality_estimate` (which is
/// a boot-time snapshot) so the screen shows a family that just crossed its cap, not one that was
/// under it when the process started.
pub async fn list_with_state(pool: &PgPool) -> Result<Vec<CatalogRow>, TelemetryError> {
    let registry = metrics::global();
    let states: std::collections::HashMap<String, metrics::FamilyState> = registry
        .family_states()
        .into_iter()
        .map(|state| (state.name.clone(), state))
        .collect();
    let budgets: std::collections::HashMap<String, usize> = metrics::FAMILIES
        .iter()
        .map(|spec| (spec.name.to_owned(), spec.max_series))
        .collect();

    let mut rows: Vec<CatalogRow> = sqlx::query_as::<_, CatalogRow>(
        "select id, name, kind, unit, description, labels, source, cardinality_estimate, \
                cardinality_budget, budgeted, last_seen_at, updated_at \
         from obs_metric_catalog order by source, name",
    )
    .fetch_all(pool)
    .await?;

    for row in &mut rows {
        if let Some(state) = states.get(&row.name) {
            row.cardinality_estimate = i32::try_from(state.series).unwrap_or(i32::MAX);
            row.cardinality_budget = i32::try_from(state.max_series).unwrap_or(i32::MAX);
        } else if let Some(budget) = budgets.get(&row.name) {
            // A family declared by an earlier build and not in the running registry: its row is
            // kept, and its cap is refreshed from the spec if the spec still exists. The screen
            // shows it as "not recorded by this build" rather than pretending it is live.
            row.cardinality_budget = i32::try_from(*budget).unwrap_or(i32::MAX);
        }
    }
    Ok(rows)
}

/// The families that have folded a sample into their overflow series, by name.
///
/// Returned beside the catalogue rather than folded into a row: a fold is a *moment* worth
/// surfacing ("this family crossed its cap in the last minute") and not a property of the family,
/// so it belongs in the response's own field where a caller can alert on it. Encoding it in the
/// row — a boolean column, a count smuggled into the description — would make the only way to read
/// it a screen-specific convention, and the next screen would invent a different one.
pub async fn over_budget_families(pool: &PgPool) -> Result<Vec<String>, TelemetryError> {
    let over: Vec<String> = metrics::global()
        .family_states()
        .into_iter()
        .filter(|state| state.over_budget)
        .map(|state| state.name)
        .collect();
    // A family that is over budget but has no row yet is a boot-ordering artefact, not a defect:
    // it will have a row on the next seed. Answering with what the registry knows is still the
    // truth, so the row set is not filtered here.
    let _ = pool;
    Ok(over)
}

/// One family by name.
pub async fn find(pool: &PgPool, name: &str) -> Result<Option<CatalogRow>, TelemetryError> {
    sqlx::query_as::<_, CatalogRow>(
        "select id, name, kind, unit, description, labels, source, cardinality_estimate, \
                cardinality_budget, budgeted, last_seen_at, updated_at \
         from obs_metric_catalog where name = $1",
    )
    .bind(name)
    .fetch_optional(pool)
    .await
    .map_err(TelemetryError::from)
}

/// How many families the catalogue holds.
pub async fn count(pool: &PgPool) -> Result<i64, TelemetryError> {
    // `count(*)` is `int8` and sqlx will not coerce it into an `i64` on its own; the cast is in
    // SQL because a 500 on a healthy database is the symptom of getting this wrong.
    let total: i64 = sqlx::query_scalar("select count(*)::int8 from obs_metric_catalog")
        .fetch_one(pool)
        .await?;
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_declaration_carries_the_spec_fields_and_the_live_count() {
        let spec = metrics::family("omnion_http_requests_total").expect("a documented family");
        let declaration = FamilyDeclaration::from_spec(spec, 12);
        assert_eq!(declaration.kind, "counter");
        assert_eq!(declaration.labels, vec!["route", "method", "status"]);
        assert_eq!(declaration.cardinality_estimate, 12);
        assert_eq!(
            declaration.cardinality_budget,
            i32::try_from(spec.max_series).unwrap()
        );
        assert!(declaration.budgeted);
    }

    #[test]
    fn an_unbudgeted_family_says_so_rather_than_implying_a_cap_it_does_not_have() {
        let spec = metrics::family("omnion_build_info").expect("a documented family");
        let declaration = FamilyDeclaration::from_spec(spec, 2);
        assert!(
            !declaration.budgeted,
            "build_info is deliberately unbounded and the row must not claim otherwise"
        );
    }

    #[test]
    fn every_declared_family_has_a_declaration_with_a_unique_name() {
        let mut names: Vec<String> = Vec::new();
        for spec in metrics::FAMILIES {
            let declaration = FamilyDeclaration::from_spec(spec, 0);
            assert!(!declaration.name.is_empty());
            assert!(matches!(
                declaration.kind.as_str(),
                "counter" | "gauge" | "histogram"
            ));
            assert!(declaration.cardinality_budget > 0);
            names.push(declaration.name);
        }
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "two families share a name");
    }

    #[test]
    fn a_custom_declaration_is_a_module_family_not_a_core_one() {
        let declaration = FamilyDeclaration::custom(
            "omnion_shopsync_orders_total",
            MetricKind::Counter,
            &["state"],
        );
        assert_eq!(declaration.source, "module");
        assert!(declaration.budgeted);
    }
}
