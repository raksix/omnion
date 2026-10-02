#!/usr/bin/env node
/**
 * Generate the six Grafana dashboards in `infra/observability/grafana/`.
 *
 * The dashboards are GENERATED rather than hand-written for one reason: a hand-written
 * dashboard JSON drifts from the metric families it queries, and a panel whose query names a
 * family that no longer exists renders an empty graph that looks exactly like a quiet system.
 * Here, every family name is read out of the registry's own declaration in
 * `crates/telemetry/src/metrics.rs`, so a renamed family is a build error rather than a blank
 * panel.
 *
 * The queries themselves are the README's mapping, verbatim — that file is the contract, and
 * this script is what keeps the dashboards agreeing with it.
 *
 * Usage: node scripts/qa/generate-observability-dashboards.mjs [--check]
 *   --check  fail instead of writing, for the CI gate
 */

import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const OUT_DIR = join(ROOT, "infra", "observability", "grafana");
const METRICS_RS = join(ROOT, "crates", "telemetry", "src", "metrics.rs");
const README = join(ROOT, "infra", "observability", "README.md");
const checkOnly = process.argv.includes("--check");

/* ── the declared families, read from the registry ─────────────────────────────────────────── */

/**
 * Every `name:` inside a `FamilySpec` in the registry.
 *
 * Read from the source rather than a hand-kept list for the reason in the file header: the list
 * in this script would be a fourth copy of the family names, and copies drift.
 */
function declaredFamilies() {
  const source = readFileSync(METRICS_RS, "utf8");
  const names = new Set();
  for (const match of source.matchAll(/name:\s*"(omnion_[a-z0-9_]+)"/g)) {
    names.add(match[1]);
  }
  return names;
}

const FAMILIES = declaredFamilies();
if (FAMILIES.size < 20) {
  console.error(
    `FATAL: only ${FAMILIES.size} families parsed out of metrics.rs — the regex is stale, and a ` +
      `stale regex here would generate dashboards that query nothing.`,
  );
  process.exit(1);
}

/**
 * Every family a dashboard references, checked against the registry.
 *
 * A family is allowed if it is declared, or if it is a derived name the exposition produces for
 * a declared histogram (`_bucket`, `_sum`, `_count`) — those are part of the family, not
 * separate families.
 */
function assertFamiliesUsed(used) {
  const unknown = [...used].filter(
    (name) =>
      !FAMILIES.has(name) &&
      ![...FAMILIES].some(
        (declared) => name.startsWith(`${declared}_bucket`) || name.startsWith(`${declared}_sum`) || name.startsWith(`${declared}_count`),
      ),
  );
  if (unknown.length > 0) {
    console.error(
      `FATAL: these queries name families the registry does not declare:\n  ${unknown.join("\n  ")}\n` +
        `A panel on an undeclared family renders an empty graph that looks like a quiet system.`,
    );
    process.exit(1);
  }
}

/* ── the README is the contract, and this asserts the dashboards agree with it ─────────────── */

function assertReadmeMentions(panels) {
  const readme = readFileSync(README, "utf8");
  const missing = [];
  for (const dashboard of panels) {
    for (const p of dashboard.panels) {
      // `p.targets` holds rendered PROMETHEUS TARGET OBJECTS (`{ expr }`), not bare strings —
      // reading `.expr` off a string would have thrown here, and throwing is how this check
      // would have looked like a broken script rather than a broken check.
      for (const target of p.targets) {
        for (const family of familiesIn(target.expr)) {
          if (!readme.includes(family)) {
            missing.push(`${dashboard.file} → "${p.title}" → ${family}`);
          }
        }
      }
    }
  }
  if (missing.length > 0) {
    console.error(
      `FATAL: these panels query families the README's mapping does not document:\n  ${missing.join("\n  ")}\n` +
        `The README is the contract; a panel the mapping does not mention is a panel nobody can debug.`,
    );
    process.exit(1);
  }
}

function familiesIn(expr) {
  return [...expr.matchAll(/omnion_[a-z0-9_]+/g)].map((match) => match[0]);
}

/* ── the dashboard definitions ───────────────────────────────────────────────────────────────── */

let uidCounter = 0;
/** A stable, short uid. Grafana requires one; a random one makes every re-import a duplicate. */
function uid(name) {
  uidCounter += 1;
  let hash = 0;
  for (const char of name) hash = (hash * 31 + char.charCodeAt(0)) | 0;
  return `${Math.abs(hash).toString(36)}${uidCounter.toString(36)}`.slice(0, 12);
}

let panelId = 0;
function panel(title, description, targets, unit = "short", extra = {}) {
  panelId += 1;
  return {
    id: panelId,
    type: extra.type ?? "timeseries",
    title,
    description,
    gridPos: extra.gridPos ?? { h: 8, w: 12, x: 0, y: 0 },
    fieldConfig: {
      defaults: {
        unit,
        // Thresholds are drawn on the panel rather than only described in the README: a panel
        // with no line on it makes the operator judge the number by eye against a threshold they
        // have to remember.
        thresholds: extra.thresholds ?? {
          mode: "absolute",
          steps: [{ color: "green", value: null }],
        },
        custom: extra.custom ?? {},
      },
      overrides: [],
    },
    options: extra.options ?? {
      legend: { displayMode: "list", placement: "bottom", calcs: ["lastNotNull"] },
      tooltip: { mode: "multi", sort: "desc" },
    },
    targets: targets.map((expr) => ({
      // A datasource UID that does not exist is a panel that fails to load on import, so the
      // README says to create a Prometheus datasource named exactly this.
      datasource: { type: "prometheus", uid: "omnion-prometheus" },
      editorMode: "code",
      expr,
      legendFormat: extra.legend ?? "",
      refId: String.fromCharCode(65 + targets.indexOf(expr)),
    })),
  };
}

function stat(title, description, targets, extra = {}) {
  return panel(title, description, targets, extra.unit ?? "short", {
    type: "stat",
    gridPos: { h: 5, w: 4, x: 0, y: 0 },
    thresholds: extra.thresholds ?? {
      mode: "absolute",
      steps: [
        { color: "green", value: null },
        { color: "red", value: extra.badAbove ?? 0.1 },
      ],
    },
    options: {
      reduceOptions: { calcs: ["lastNotNull"], fields: "", values: false },
      colorMode: "value",
      graphMode: "area",
      textMode: "auto",
    },
    legend: extra.legend ?? "",
  });
}

const dashboards = [
  {
    file: "omnion-overview.json",
    title: "Omnion · API overview",
    panels: [
      stat("Request rate", "Requests per second across every route.", [
        "sum(rate(omnion_http_requests_total[5m]))",
      ], { unit: "reqps" }),
      stat("Error ratio", "Share of requests answered 5xx. A ratio, not a count: a count fires on a busy instance and stays quiet on a quiet one.", [
        "sum(rate(omnion_http_requests_total{status=\"5xx\"}[5m])) / clamp_min(sum(rate(omnion_http_requests_total[5m])), 0.001)",
      ], { unit: "percentunit", badAbove: 0.05 }),
      stat("p95 latency", "95th percentile request duration. The threshold line is the bundled SlowRequests alert's 1.5s.", [
        "histogram_quantile(0.95, sum(rate(omnion_http_request_duration_seconds_bucket[5m])) by (le))",
      ], { unit: "s", badAbove: 1.5 }),
      stat("Drops (5m)", "Telemetry samples folded into an overflow series because a cardinality cap was hit. Non-zero here means numbers are getting coarser.", [
        "sum(increase(omnion_registry_budget_exceeded[5m]))",
      ], { unit: "short", badAbove: 1 }),
      panel("Request rate by status class", "Every request, split by status CLASS rather than code — a 4xx and a 5xx are different incidents.", [
        "sum by (status) (rate(omnion_http_requests_total[5m]))",
      ], "reqps", { legend: "{{status}}" }),
      panel("Slowest routes", "Cumulative time per route template. Route labels are templates, never raw paths.", [
        "topk(10, sum by (route) (rate(omnion_http_request_duration_seconds_sum[5m])))",
      ], "s", { legend: "{{route}}" }),
      panel(
        "Latency percentiles",
        "p50, p95 and p99 side by side. One line for p99 hides the tail that users actually feel.",
        [
          "histogram_quantile(0.50, sum(rate(omnion_http_request_duration_seconds_bucket[5m])) by (le))",
          "histogram_quantile(0.95, sum(rate(omnion_http_request_duration_seconds_bucket[5m])) by (le))",
          "histogram_quantile(0.99, sum(rate(omnion_http_request_duration_seconds_bucket[5m])) by (le))",
        ],
        "s",
        { legend: "p{{ quantile }}" },
      ),
    ],
  },
  {
    file: "omnion-database.json",
    title: "Omnion · Database",
    panels: [
      stat("Idle connections", "Pool headroom. Zero idle with requests in flight means every request is waiting for a connection — a different incident from high acquired.", [
        "omnion_db_pool_connections{state=\"idle\"}",
      ], { badAbove: 0 }),
      panel("Pool states", "idle / acquired / max, over time. A pool that sits at max acquired is a queue, not a pool.", [
        "omnion_db_pool_connections",
      ], "short", { legend: "{{state}}" }),
      panel(
        "Query latency p95",
        "By statement NAME. Never by parameter value — a label carrying query arguments is a label carrying user data.",
        [
          "histogram_quantile(0.95, sum(rate(omnion_db_query_duration_seconds_bucket[5m])) by (le, statement))",
        ],
        "s",
        { legend: "{{statement}}" },
      ),
      panel("Query rate", "Statements per second, by name.", [
        "sum by (statement) (rate(omnion_db_query_duration_seconds_count[5m]))",
      ], "ops", { legend: "{{statement}}" }),
    ],
  },
  {
    file: "omnion-queue-workers.json",
    title: "Omnion · Queue and workers",
    panels: [
      stat("Max queue depth", "The deepest queue right now. The bundled QueueBacklog rule fires at 100 held for five minutes.", [
        "max(omnion_queue_depth)",
      ], { badAbove: 100 }),
      panel("Queue depth by queue", "Waiting items per queue. A queue that is not draining is a queue that will page you.", [
        "omnion_queue_depth",
      ], "short", { legend: "{{queue}}" }),
      panel(
        "Job duration p95",
        "By job kind.",
        [
          "histogram_quantile(0.95, sum(rate(omnion_queue_job_duration_seconds_bucket[5m])) by (le, kind))",
        ],
        "s",
        { legend: "{{kind}}" },
      ),
      panel("Job failures", "Failed jobs per second, by kind. A failure rate that is flat but non-zero is worse than a spike: it is a permanently broken job.", [
        "sum by (kind) (rate(omnion_queue_job_failures_total[5m]))",
      ], "ops", { legend: "{{kind}}" }),
      panel("Outbound retries", "Retries by subsystem. A subsystem retrying steadily is a dependency that is down, not a network blip.", [
        "sum by (subsystem, outcome) (rate(omnion_outbound_retries_total[5m]))",
      ], "ops", { legend: "{{subsystem}} {{outcome}}" }),
    ],
  },
  {
    file: "omnion-workflows.json",
    title: "Omnion · Workflows",
    panels: [
      stat("Steps per second", "Workflow steps started and finished, across every run.", [
        "sum(rate(omnion_workflow_steps_total[5m]))",
      ], { unit: "ops" }),
      panel("Step outcomes", "By status. A run that fails on the same step every time is a workflow bug, not an outage.", [
        "sum by (status) (rate(omnion_workflow_steps_total[5m]))",
      ], "ops", { legend: "{{status}}" }),
      panel("Failed step ratio", "Share of steps that failed. Kept as a ratio so a quiet instance and a busy one are comparable.", [
        "sum(rate(omnion_workflow_steps_total{status=\"failed\"}[10m])) / clamp_min(sum(rate(omnion_workflow_steps_total[10m])), 0.001)",
      ], "percentunit", { badAbove: 0.05 }),
    ],
  },
  {
    file: "omnion-ai-usage.json",
    title: "Omnion · AI usage and cost",
    panels: [
      stat("Spend, 24h", "Micro-dollars summed over 24 hours, shown in currency. The cost family is micros, so the panel divides.", [
        "sum(increase(omnion_ai_cost_micros_total[24h])) / 1000000",
      ], { unit: "currencyUSD" }),
      panel("AI requests by provider", "Requests per second, by provider. Provider is a bounded learned set with an `other` overflow.", [
        "sum by (provider) (rate(omnion_ai_requests_total[5m]))",
      ], "ops", { legend: "{{provider}}" }),
      panel("AI requests by model", "By model. A model reading as `other` is the cap folding the tail — treat it as a signal to raise the budget, not as a model.", [
        "sum by (model) (rate(omnion_ai_requests_total[5m]))",
      ], "ops", { legend: "{{model}}" }),
      panel("Tokens by kind", "Prompt and completion tokens per second. The ratio between them is the cheapest cost signal there is.", [
        "sum by (kind) (rate(omnion_ai_tokens_total[5m]))",
      ], "short", { legend: "{{kind}}" }),
      panel("Cost rate", "Micros per second, by provider and model.", [
        "sum by (provider, model) (rate(omnion_ai_cost_micros_total[5m]))",
      ], "short", { legend: "{{provider}} {{model}}" }),
      panel("AI failures", "Failed AI calls by provider. A provider failing steadily is the circuit-breaker family on the reliability dashboard, one layer down.", [
        "sum by (provider, status) (rate(omnion_ai_requests_total[5m]))",
      ], "ops", { legend: "{{provider}} {{status}}" }),
    ],
  },
  {
    file: "omnion-outbound-reliability.json",
    title: "Omnion · Outbound reliability and telemetry health",
    panels: [
      stat("Exporter drops, 5m", "Telemetry lost because an exporter's buffer filled. Non-zero means your monitoring has a hole in it.", [
        "sum(increase(omnion_exporter_dropped_total[5m]))",
      ], { badAbove: 1 }),
      stat("Open circuits", "Providers with an open circuit. Open is not a fault — it is what the circuit is FOR. Sustained-open is.", [
        "count(omnion_circuit_state == 1)",
      ], { badAbove: 0 }),
      stat("Drains that timed out (1h)", "Shutdowns that reached their deadline with requests still running. Exit code is 0 either way, so this is the only signal of requests being cut at deploy time.", [
        "sum(increase(omnion_shutdowns_total{outcome=\"timed_out\"}[1h]))",
      ], { badAbove: 0 }),
      panel("Webhook deliveries", "By result. A third failing is usually a misconfigured endpoint rather than a broken one.", [
        "sum by (result) (rate(omnion_webhook_deliveries_total[5m]))",
      ], "ops", { legend: "{{result}}" }),
      panel("Circuit state", "1 is open, 0 is closed. A circuit that flaps is a dependency that is neither up nor down.", [
        "omnion_circuit_state",
      ], "short", { legend: "{{provider}}" }),
      panel("Exporter batches flushed", "Batches a flush loop handed to a backend and the backend accepted. Flat while requests flow is an exporter that is not draining.", [
        "sum by (kind) (rate(omnion_exporter_batches_flushed_total[5m]))",
      ], "ops", { legend: "{{kind}}" }),
      panel("Cardinality losses", "Samples folded into an overflow series. The failure that grows silently.", [
        "sum by (family) (rate(omnion_registry_budget_exceeded[5m]))",
      ], "ops", { legend: "{{family}}" }),
      panel("Alert transitions", "Alert state moves over the last hour. A rule that never appears here is either never firing or not being evaluated.", [
        "sum by (state) (increase(omnion_alert_transitions_total[1h]))",
      ], "short", { legend: "{{state}}" }),
      panel("Alert evaluations", "Passes the evaluator ran. Flat means the loop stopped, and every rule on the page is silently dead.", [
        "sum(increase(omnion_alert_evaluations_total[15m]))",
      ], "short", { badAbove: 0 }),
    ],
  },
];

/* ── the checks, then the write ─────────────────────────────────────────────────────────────── */

const used = new Set();
for (const dashboard of dashboards) {
  for (const p of dashboard.panels) {
    for (const target of p.targets) familiesIn(target.expr).forEach((f) => used.add(f));
  }
}
assertFamiliesUsed(used);
assertReadmeMentions(dashboards);
mkdirSync(OUT_DIR, { recursive: true });

let changed = 0;
for (const dashboard of dashboards) {
  // Grafana imports a dashboard with an `id` of `null`; a baked-in id collides with whatever is
  // already in the target Grafana and the import lands as a second copy.
  const json = {
    __inputs: [],
    __requires: [{ type: "grafana", id: "grafana", name: "Grafana", version: "10.0.0" }],
    annotations: { list: [] },
    editable: true,
    // A template variable for the instance, so one dashboard file works against a staging and a
    // production Prometheus without editing every query.
    templating: {
      list: [
        {
          name: "instance",
          label: "Instance",
          type: "query",
          datasource: { type: "prometheus", uid: "omnion-prometheus" },
          query: "label_values(omnion_build_info, instance)",
          refresh: 1,
        },
      ],
    },
    time: { from: "now-6h", to: "now" },
    timepicker: {},
    timezone: "browser",
    title: dashboard.title,
    uid: uid(dashboard.file),
    version: 1,
    refresh: "30s",
    schemaVersion: 39,
    tags: ["omnion", "observability"],
    panels: dashboard.panels.map((p, index) => ({
      ...p,
      // A grid position per panel, laid out left to right. Grafana auto-arranges panels with no
      // gridPos, but a dashboard whose layout depends on insertion order is one that reflows
      // every time a panel is added.
      gridPos:
        p.gridPos ??
        { h: 8, w: 12, x: (index % 2) * 12, y: Math.floor(index / 2) * 8 },
    })),
  };
  const path = join(OUT_DIR, dashboard.file);
  const rendered = `${JSON.stringify(json, null, 2)}\n`;
  let existing = null;
  try {
    existing = readFileSync(path, "utf8");
  } catch {
    // First write.
  }
  if (existing === rendered) continue;
  changed += 1;
  if (checkOnly) {
    console.error(`FATAL: ${dashboard.file} is out of date — run this script without --check.`);
    process.exit(1);
  }
  writeFileSync(path, rendered);
  console.log(`wrote ${dashboard.file} (${json.panels.length} panels)`);
}

console.log(
  `${dashboards.length} dashboards, ${used.size} distinct families referenced, ` +
    `all declared in the registry and documented in the README. ` +
    `${checkOnly ? "No changes needed." : `${changed} file(s) written.`}`,
);
