# Omnion observability bundle

Omnion **emits** telemetry and ships the assets to read it. It is not a replacement for your
Grafana, Prometheus, Tempo, Jaeger or Loki — the request is explicit about this, and the
`/observability/exporters` screen says so on the row itself. Point the exporters at your own
backend and import what is here.

| | |
|---|---|
| Bundle version | `1.0.0` |
| Built for instance | `0.1.x` |
| Scrapes | `GET /metrics` — unauthenticated on the internal interface |

## The four steps, in the order they fail

1. **Metrics → Prometheus.** Add the instance to your scrape config:

   ```yaml
   scrape_configs:
     - job_name: omnion
       static_configs:
         - targets: ['omnion-api:8080']
       # `/metrics` is unauthenticated and carries no secrets, but it does carry route
       # templates and provider names. Keep it on the internal network, or set
       # `prometheus_public` in the settings screen and put a token or CIDR allow-list in front.
   ```

2. **Traces and logs → a collector.** `otel-collector.yaml` in this directory is a working
   example: it receives OTLP, batches, and forwards to whatever backend you run. Change the two
   `exporters:` endpoints and it is yours.

3. **Dashboards → Grafana.** The six JSON files below import through *Dashboards → Import* with no
   datasource editing; they read the `Prometheus` datasource by that name.

4. **Alerts → either side.** The `alerts.yml` rules are the same four rules the panel seeds
   (`omnion_alert_rules`, `source = 'bundled'`). Run them in **Prometheus** if you want
   Alertmanager's routing; run them in **Omnion** if you want the panel's timeline and silences.
   Running both means two notifications per incident, so `OMNION_ALERTS_EVALUATOR=false` turns off
   the built-in loop when the Prometheus rules are the ones you rely on.

## Every panel, and the families it queries

This mapping is the point of the bundle: a panel whose query names a family that does not exist is
a panel that shows an empty graph and looks like a quiet system.

### `omnion-overview.json` — API overview

| Panel | Query | Families |
|---|---|---|
| Request rate | `sum(rate(omnion_http_requests_total[5m]))` | `omnion_http_requests_total` |
| Error ratio | `sum(rate(omnion_http_requests_total{status="5xx"}[5m])) / sum(rate(omnion_http_requests_total[5m]))` | `omnion_http_requests_total` |
| p95 latency | `histogram_quantile(0.95, sum(rate(omnion_http_request_duration_seconds_bucket[5m])) by (le))` | `omnion_http_request_duration_seconds` |
| Requests by status class | `sum by (status) (rate(omnion_http_requests_total[5m]))` | `omnion_http_requests_total` |
| Slowest routes | `topk(10, sum by (route) (rate(omnion_http_request_duration_seconds_sum[5m])))` | `omnion_http_request_duration_seconds` |

Route labels are **templates** (`/api/v1/secrets/{id}`, never the literal id) — a raw path would
put one series per secret id into the registry, which is exactly the cardinality blow-up the
budget in the settings screen exists to prevent.

### `omnion-database.json`

| Panel | Query | Families |
|---|---|---|
| Pool states | `omnion_db_pool_connections` | `omnion_db_pool_connections` |
| Query latency | `histogram_quantile(0.95, sum(rate(omnion_db_query_duration_seconds_bucket[5m])) by (le, statement))` | `omnion_db_query_duration_seconds` |

`statement` is a **statement name**, never a parameter value. A label carrying query arguments is
a label carrying user data.

### `omnion-queue-workers.json`

| Panel | Query | Families |
|---|---|---|
| Queue depth | `omnion_queue_depth` | `omnion_queue_depth` |
| Job duration | `histogram_quantile(0.95, sum(rate(omnion_queue_job_duration_seconds_bucket[5m])) by (le, kind))` | `omnion_queue_job_duration_seconds` |
| Job failures | `sum by (kind) (rate(omnion_queue_job_failures_total[5m]))` | `omnion_queue_job_failures_total` |
| Outbound retries | `sum by (subsystem) (rate(omnion_outbound_retries_total[5m]))` | `omnion_outbound_retries_total` |

### `omnion-workflows.json`

| Panel | Query | Families |
|---|---|---|
| Step outcomes | `sum by (status) (rate(omnion_workflow_steps_total[5m]))` | `omnion_workflow_steps_total` |
| Step throughput | `sum(rate(omnion_workflow_steps_total[5m]))` | `omnion_workflow_steps_total` |

### `omnion-ai-usage.json`

| Panel | Query | Families |
|---|---|---|
| Requests by provider | `sum by (provider) (rate(omnion_ai_requests_total[5m]))` | `omnion_ai_requests_total` |
| Tokens by kind | `sum by (kind) (rate(omnion_ai_tokens_total[5m]))` | `omnion_ai_tokens_total` |
| Spend today | `sum(increase(omnion_ai_cost_micros_total[24h])) / 1e6` | `omnion_ai_cost_micros_total` |

Model labels come from a bounded learned set with an `other` overflow bucket. A panel that reads
`model="other"` is reading the aggregate of everything the cap folded — treat it as a signal to
raise the budget, not as a model.

### `omnion-outbound-reliability.json`

| Panel | Query | Families |
|---|---|---|
| Webhook results | `sum by (result) (rate(omnion_webhook_deliveries_total[5m]))` | `omnion_webhook_deliveries_total` |
| Circuit state | `omnion_circuit_state` | `omnion_circuit_state` |
| Exporter health | `omnion_exporter_batches_flushed_total` | `omnion_exporter_batches_flushed_total` |
| Dropped telemetry | `sum by (exporter) (rate(omnion_exporter_dropped_total[5m]))` | `omnion_exporter_dropped_total` |
| Cardinality losses | `omnion_registry_budget_exceeded` | `omnion_registry_budget_exceeded` |
| Alert transitions | `sum by (state) (increase(omnion_alert_transitions_total[1h]))` | `omnion_alert_transitions_total` |
| Shutdowns | `sum by (outcome) (increase(omnion_shutdowns_total[24h]))` | `omnion_shutdowns_total` |

## The two families that are about Omnion rather than about your traffic

`omnion_registry_budget_exceeded` and `omnion_exporter_dropped_total` exist because the two ways
telemetry goes quietly wrong are **losing data** and **growing without bound**, and both are
invisible until you look for them. A non-zero budget-exceeded rate means samples are being folded
into an `other` series; a rising drop counter means a backend was unreachable and the buffer
evicted its oldest entries. Neither is an error the API can raise, because neither breaks a
request — which is exactly why they are metrics and not log lines.

## What leaves the instance when you configure an exporter

Stated here because the settings screen has to say it out loud and this is the sentence it says:

- **Log lines and spans.** Every line the request-log middleware writes, redacted at construction.
- **Route templates, status classes, durations.** Never raw paths, never query strings.
- **AI provider and model names, token counts, cost.** Never prompt or completion text — the span
  builder has no field a prompt could travel through, so a caller cannot leak one by forgetting
  to redact.
- **Host name and instance version.** Useful for attributing a spike to a release.

And what does not, because it is redacted before the payload is built: secret values, e-mail
addresses, request and session token values, and every field whose *name* matches the redaction
list. The test that proves it is in the crate, not here — it renders a flush body with a fixture
secret and a fixture e-mail in it and greps the output.

## Compatibility

The bundle is versioned separately from the instance. A dashboard written against
`omnion_http_request_duration_seconds_sum` breaks if a family is renamed; when that happens the
`/observability/metrics/catalog` endpoint answers with the families **this build** emits, and the
`/observability/bundle` endpoint answers with the bundle version it was built for. Compare the
two before importing: a mismatch means the dashboards need their queries updated, and the
catalogue tells you what the new names are.
