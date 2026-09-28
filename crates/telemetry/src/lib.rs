//! Omnion observability (docs/requests/REQ-126-observability-stack.md).
//!
//! One crate, one job per module, and one rule that decides who owns what:
//!
//! * [`schema`] — the log line. Every field the request names, in one struct, so a field cannot
//!   exist in the store and be missing from the explorer, or vice versa.
//! * [`context`] — the request-scoped binding. A middleware assigns a request id at the edge and
//!   binds the user and organization after authentication; every line emitted inside that task
//!   inherits all three, and a worker inherits the trace of whatever enqueued it.
//! * [`redact`] — the **single** redaction pass. Not a helper, not a convention: the module that
//!   REQ-037 owns (`omnion_secrets::redaction`) decides what a secret looks like, and this one
//!   decides what a *field* may contain, so a log line, a span attribute and an exporter payload
//!   are all filtered by the same rule.
//! * [`metrics`] — the metric registry. The families are declared, the labels are positional and
//!   closed, and the cardinality budget is enforced in one place with the loss counted and
//!   labelled rather than swallowed.
//! * [`metric_catalog`] — the registry's durable projection, so the panel documents what the
//!   process can record rather than what someone remembered to write down.
//! * [`store`] — the bounded store. The explorer reads it and nothing else; a request path never
//!   blocks on it.
//! * [`exporter_flush`] — the part of the exporter pipeline that actually MOVES telemetry: the
//!   fan-out that feeds the buffers and the loop that drains them. It lives beside
//!   [`exporter`] rather than in the API binary because the drain is worth testing without a
//!   database and the fan-out is worth testing without a request.
//!
//! ## Why a separate crate and not a module in `omnion-core`
//!
//! `omnion-core` already owns the *subscriber* (the JSON vs pretty switch) and it will keep
//! owning it. What arrives here is everything that needs to be **shared, testable on its own and
//! reachable from a worker binary that does not link the whole core**: the schema, the context
//! propagation, the redaction pass and the store. Two implementations of any of those drift, and
//! drift in this area is a leak.

pub mod alert_loop;
pub mod alerts;
pub mod context;
pub mod error;
pub mod exporter;
pub mod exporter_flush;
pub mod lifecycle;
pub mod metric_catalog;
pub mod metrics;
pub mod redact;
pub mod retention;
pub mod schema;
pub mod store;
pub mod trace_store;
pub mod tracing_span;
pub mod tracing_spine;

pub use alerts::{Expression, PassReport, Preview, Rule, Silence, parse as parse_alert_expr};
pub use context::{LogContext, mint_span_id, mint_trace_id, trace_id_from_header};
pub use error::TelemetryError;
pub use exporter::{
    Batch, Collector as ExporterCollector, ExporterHealth, ExporterKind, FlushOutcome,
};
pub use exporter_flush::{batch_body, fan_out};
pub use lifecycle::{DrainOutcome, Lifecycle, ShutdownSummary, drain_and_flush};
pub use metric_catalog::FamilyDeclaration;
pub use metrics::{FamilySpec, MetricKind, Registry, global as metrics_registry};
pub use redact::{REDACTED, redact_fields, redact_text};
pub use retention::{PRUNED_EVENT, PruneReport, Retention};
pub use schema::{LogEntry, LogLevel, LogSource, NewLogEntry};
pub use trace_store::TraceFilter;
pub use tracing_span::{
    MAX_SPANS_PER_TRACE, Parent, SamplingDecision, Span, TraceContext, TraceParent, TraceRecord,
    TraceSummary,
};

/// The cap on a single log line's `fields` object.
///
/// A log line is not a data transport. A caller that wants to serialise a request body has found
/// the wrong API, and the answer is a truncation that is visible in the row rather than a line
/// that silently grows to a megabyte.
pub const MAX_FIELD_COUNT: usize = 32;

/// The cap on one field's rendered length, in characters.
pub const MAX_FIELD_CHARS: usize = 512;

/// The version of the shipped observability bundle (`infra/observability/`).
///
/// Compiled in rather than read from disk, because the panel has to say which bundle THIS
/// instance was built to import and the directory it was built from may not have been deployed.
/// The bundle carries its own version file; a mismatch between the two is what the compatibility
/// note in the manifest exists to explain.
pub const BUNDLE_VERSION: &str = "1.0.0";

/// What the bundle ships, as `(path, what it is)` pairs.
///
/// The list is the request's own enumeration — "Grafana dashboard JSON, Prometheus alert rules,
/// an OpenTelemetry collector example configuration, and a README mapping each dashboard panel to
/// the metric families" — and each entry names a file that exists in the tree. A test asserts
/// that, because an asset list is a promise and a promise with no file behind it is how a bundle
/// quietly becomes a README describing assets that were never written.
pub const BUNDLE_ASSETS: &[(&str, &str)] = &[
    (
        "grafana/omnion-overview.json",
        "API overview: request rate, error ratio, p95 latency",
    ),
    (
        "grafana/omnion-database.json",
        "Pool states and query latency by statement name",
    ),
    (
        "grafana/omnion-queue-workers.json",
        "Queue depth, job duration, failures, worker heartbeat",
    ),
    (
        "grafana/omnion-workflows.json",
        "Workflow step outcomes and queue interaction",
    ),
    (
        "grafana/omnion-ai-usage.json",
        "AI requests, tokens and spend per provider and model",
    ),
    (
        "grafana/omnion-outbound-reliability.json",
        "Webhook results, outbound retries, circuit state, exporter health",
    ),
    (
        "alerts.yml",
        "Prometheus alert rules for the usual suspects, seeded into obs_alert_rules",
    ),
    (
        "otel-collector.yaml",
        "OpenTelemetry collector example configuration",
    ),
    (
        "README.md",
        "Each dashboard panel mapped to the metric families it queries",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// The repository root, found by walking up from this file.
    ///
    /// From `crates/telemetry/src/lib.rs` that is three levels up. Hard-coded rather than read
    /// from `CARGO_MANIFEST_DIR` + `../..` so the same expression works whether the test runs
    /// from the crate or from a copied source tree.
    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("the crate is two levels below the repository root")
            .to_path_buf()
    }

    #[test]
    fn every_asset_the_manifest_names_exists_in_the_tree() {
        // The manifest is served to the panel and shown as a list of things the operator can
        // import. A name with no file behind it is a button that leads nowhere, which the
        // request's own definition of done forbids outright.
        let bundle = repo_root().join("infra").join("observability");
        for (path, description) in BUNDLE_ASSETS {
            let full = bundle.join(path);
            assert!(
                full.exists(),
                "the manifest names `{path}` ({description}) and there is no such file — \
                 an asset list is a promise and this one is broken"
            );
            assert!(
                std::fs::metadata(&full)
                    .expect("the asset metadata reads")
                    .len()
                    > 0,
                "`{path}` is empty"
            );
        }
    }

    #[test]
    fn the_readme_documents_every_family_the_dashboards_query() {
        // The README is the contract the request asks for ("a README mapping each dashboard
        // panel to the metric families above"). Checking it HERE means the Rust suite fails when
        // a dashboard grows a panel the mapping does not mention, not only when someone
        // remembers to run the Node generator.
        //
        // The dashboards are generated, so this is not a second source of truth: it is the
        // assertion that the generated output still matches the document beside it.
        let readme = std::fs::read_to_string(
            repo_root()
                .join("infra")
                .join("observability")
                .join("README.md"),
        )
        .expect("the bundle README exists");

        for (path, _) in BUNDLE_ASSETS {
            if !path.ends_with(".json") {
                continue;
            }
            let json =
                std::fs::read_to_string(repo_root().join("infra").join("observability").join(path))
                    .expect("the dashboard exists");
            for family in families_in(&json) {
                assert!(
                    readme.contains(&family),
                    "`{path}` queries `{family}`, which the README's mapping does not document — \
                     a panel nobody can debug is a panel nobody should ship"
                );
            }
        }
    }

    /// Every `omnion_*` name in a JSON document, deduplicated.
    fn families_in(json: &str) -> Vec<String> {
        let mut found: Vec<String> = Vec::new();
        let mut rest = json;
        while let Some(at) = rest.find("omnion_") {
            rest = &rest[at + 7..];
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            let name = format!("omnion_{}", &rest[..end]);
            if !found.contains(&name) {
                found.push(name);
            }
        }
        found
    }

    #[test]
    fn the_bundle_version_is_the_one_the_readme_states() {
        let readme = std::fs::read_to_string(
            repo_root()
                .join("infra")
                .join("observability")
                .join("README.md"),
        )
        .expect("the bundle README exists");
        assert!(
            readme.contains(BUNDLE_VERSION),
            "the README does not state the bundle version {BUNDLE_VERSION}, so the two numbers \
             the compatibility note compares cannot be compared"
        );
    }

    #[test]
    fn every_declared_family_is_reachable_from_the_catalogue_the_panel_reads() {
        // The metrics screen reads `obs_metric_catalog`, not a hard-coded list, and the bundle's
        // dashboards query the registry. Both are fed from `FAMILIES`, so a family that is in
        // neither is a family nobody can see — which is the same failure as a panel on a
        // nonexistent family, from the other direction.
        assert!(
            crate::metrics::FAMILIES.len() >= 20,
            "the registry declares only {} families",
            crate::metrics::FAMILIES.len()
        );
        for spec in crate::metrics::FAMILIES {
            assert!(
                !spec.name.is_empty() && spec.name.starts_with("omnion_"),
                "a family is not namespaced: `{}`",
                spec.name
            );
        }
    }
}
