# REQ-035 — Edge / Multi-region

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** platform / infra
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

Deployment support for enterprise customers across regions:

```text
Turkey
 └── Ankara

Europe
 └── Frankfurt

US
 └── Virginia
```

Global load balancing + region-aware storage.

## Notes

- Region-aware storage must respect data-residency requirements (Compliance Center, REQ-038).

## Implementation spec

### Scope (in / out)

In:

- A region registry — `tr-ankara`, `eu-frankfurt`, `us-virginia` to start, extensible — with display name, status, per-service endpoints, storage bucket label and a default flag.
- Per-region health checking across api, admin, web, workers, database, object storage and cache, exposed as a matrix with history.
- Global load balancing: host-based and country-based routing rules with a failover order, health thresholds and a lookup simulator.
- Region-aware storage: every organization has a home region; media and file objects are written to that region's bucket, and only cacheable derivatives may replicate to configured replica regions.
- Per-organization residency policy: home region, storage region, permitted replica regions, cross-region caching allowed or not, and a policy export for auditors.
- A guarded residency migration workflow to move an organization's data between regions (planned steps, verified copy, cutover, rollback point).
- Failover runbook: drain a region, promote reads and writes to another region, plus a dry-run that reports impact without changing routing.
- A latency matrix between regions to inform routing defaults.

Out:

- Active-active multi-master writes: v1 is single-writer per organization with regional read caches.
- Automatic per-request data migration or sharding decisions.
- Air-gapped and offline deployments (REQ-036).
- CDN provider configuration and cache-rule definitions (REQ-011) — this REQ consumes the edge but does not define its policy.
- Per-region cost allocation and billing.
- Provisioning infrastructure: the registry describes regions the deployment already provides; the admin surface never creates infrastructure.

### Screens (UI)

Routes (`apps/admin/app/platform/regions/*`, feature dir `apps/admin/features/regions/`):

```text
/platform/regions                 ← region table + latency matrix
/platform/regions/routing         ← routing policy editor + simulator
/platform/regions/failover        ← failover plan, dry-run, execute
/platform/regions/{code}          ← region detail
/settings/data-residency          ← organization residency policy
/settings/data-residency/migrate  ← migration wizard and status
```

- `/platform/regions` table columns: Region, Code, Status, Home for (organization count), Services (badge row for api/admin/web/worker/database/storage/cache), p95 latency 30d, Traffic share, Last check, Open. Expanding a row reveals the per-service health table (service, status, latency, last change). Filters: status, service, default. Sorting by latency and organization count.
- Latency matrix: a region × region grid with icon, label and numeric p95 values (never colour-only) plus a `measured at` timestamp so stale data is obvious.
- Region detail: header with status chip and `Set as default` (admin only), a services table with 24h sparklines, health-check history (last 50 changes), and a config summary card listing public endpoint host names per service and the bucket label — host names only, no credentials.
- Routing editor: default region select; an ordered rules table (Priority, Match: country / host suffix / path prefix, Target region, Enabled) with drag-to-reorder and inline validation (unique priorities, no rule without a match, unknown target blocked); a failover order list (primary → secondary → tertiary) with health thresholds (consecutive failures, latency ceiling); and a `Simulate` panel taking country code, host and path, printing the chosen region and the matched rule.
- Failover surface: current state (which region is primary), a plan editor (mode `drain` or `promote`, target region, note), `Dry-run` rendering the full impact list (affected organizations, hostname changes, expected downtime estimate, cache invalidation scope), and `Execute` behind a typed confirmation of the source region code plus a second-approver field when policy requires it. A live panel shows the failover step log.
- Residency screen: current home region (with a lock note once data exists), storage region, permitted replica regions (multi-select with an audit-only note), a cross-region caching toggle whose label states the consequence in plain words ("cacheable derivatives may be served from these regions"), a policy JSON viewer with copy, and `Plan a region migration`. A storage region that is neither the home region nor a listed replica is rejected with an explanation naming the missing entry.
- Migration wizard: target region → prerequisites checklist (recent backup, no active failover, maintenance window) → copy plan with per-entity sizes and an estimated duration → cutover with typed confirmation → status timeline (copying, verifying, cutover, done or failed) with per-step progress and a rollback point. The organization shows a read-only banner during cutover.
- Empty and degraded states: a single-region deployment renders one region with an explainer that multi-region routing is inactive while the surfaces stay visible with actions disabled; missing health data shows `No recent checks` rather than a zero; an unreachable control plane shows `Unknown`, never green. Simulator errors state the reason (unknown host, malformed country code).
- Keyboard: `Ctrl+K` covers `Regions`, `Routing policy`, `Simulate routing` and `Data residency`; `g r` regions, `g s` simulate; in the routing table `j`/`k` move, `Enter` edits, `Space` toggles enabled.
- Mobile: tables become cards, routing reordering uses up/down buttons instead of drag, the dry-run impact list scrolls with a sticky execute bar, and typed confirmations offer a copy-the-code helper.

### API

| Method | Path | Purpose | Permission |
| --- | --- | --- | --- |
| GET | `/api/v1/regions` | Region list with status and traffic share | `platform.regions.read` |
| GET | `/api/v1/regions/{code}` | Region detail: services, endpoints, config summary | `platform.regions.read` |
| GET | `/api/v1/regions/health` | Health matrix with history | `platform.regions.read` |
| GET | `/api/v1/regions/latency-matrix` | Region-to-region p95 latency | `platform.regions.read` |
| PATCH | `/api/v1/regions/{code}` | Rename, set maintenance status, set as default | `platform.regions.manage` |
| GET | `/api/v1/regions/routing-policy` | Default region, rules, failover order, thresholds | `platform.routing.read` |
| PUT | `/api/v1/regions/routing-policy` | Replace the policy as a validated whole | `platform.routing.manage` |
| POST | `/api/v1/regions/routing-policy/simulate` | Resolve country/host/path to a region | `platform.routing.read` |
| GET | `/api/v1/organizations/{id}/data-residency` | Effective residency policy | `platform.residency.read` |
| PATCH | `/api/v1/organizations/{id}/data-residency` | Update replica regions and caching (home region moves only by migration) | `platform.residency.manage` |
| POST | `/api/v1/organizations/{id}/residency-migrations` | Plan a migration to a target region | `platform.residency.migrate` |
| GET | `/api/v1/organizations/{id}/residency-migrations/{mid}` | Migration status and step log | `platform.residency.read` |
| POST | `/api/v1/regions/failover/dry-run` | Impact report for a drain or promote plan | `platform.failover.plan` |
| POST | `/api/v1/regions/failover` | Execute a failover | `platform.failover.execute` |
| GET | `/api/v1/regions/failover/{id}` | Failover status and step log | `platform.regions.read` |

Conventions: reads are cached for 30 seconds and are safe for dashboards; policy writes replace the whole document so a concurrent editor cannot half-apply a table; failover and residency-migration writes require `Idempotency-Key`.

### Data model

Migration `database/migrations/0015_regions.sql`.

`regions`

| Column | Type | Notes |
| --- | --- | --- |
| `code` | `text pk` | e.g. `tr-ankara`, `eu-frankfurt`, `us-virginia` |
| `display_name` | `text not null` | operator-facing label |
| `country_group` | `text not null` | `tr`, `eu`, `us` for grouping and defaults |
| `status` | `text not null` | check in (`healthy`,`degraded`,`down`,`maintenance`) |
| `api_endpoint` | `text not null` | public host |
| `admin_endpoint` / `web_endpoint` | `text` | public hosts |
| `storage_bucket` | `text not null` | bucket label for the region |
| `cache_namespace` | `text not null` | cache key prefix |
| `is_default` | `boolean not null default false` | exactly one true (partial unique index) |
| `is_active` | `boolean not null default true` | inactive keeps history but stops routing |
| `traffic_share` | `numeric(5,2)` | computed from the routing policy, cached |
| `created_at` / `updated_at` | `timestamptz not null default now()` | |

Indexes: partial unique on `is_default` where `is_default`, `(status)`, `(country_group)`.

`region_health_checks`: `id bigserial pk`, `region_code text not null` fk `regions`, `service text not null` check in (`api`,`admin`,`web`,`worker`,`database`,`storage`,`cache`), `status text not null` check in (`healthy`,`degraded`,`down`,`unknown`), `latency_ms integer`, `checked_at timestamptz not null default now()`, `detail jsonb`. Indexes: `(region_code, service, checked_at desc)`, `(status, checked_at desc)`. Retention 30 days at one row per service per minute.

`organization_residency_policies`: `organization_id uuid pk` fk `organizations`, `home_region_code text not null` fk `regions`, `storage_region_code text not null` fk `regions`, `replica_region_codes text[] not null default '{}'`, `allow_cross_region_cache boolean not null default false`, `updated_by uuid not null`, `updated_at timestamptz not null default now()`. The service layer enforces `storage_region_code` is either the home region or a member of `replica_region_codes`.

`routing_rules`: `id uuid pk`, `priority integer not null`, `organization_id uuid` (null means global), `match jsonb not null` (`{country, host_suffix, path_prefix}`), `target_region_code text not null` fk `regions`, `enabled boolean not null default true`, `created_by uuid not null`, `created_at`, `updated_at`. Indexes: unique `(priority)`, `(organization_id)`, `(target_region_code)`.

`routing_policy` (single row): `id boolean pk default true`, `default_region_code text not null` fk `regions`, `failover_order text[] not null`, `failure_threshold integer not null default 3`, `latency_ceiling_ms integer not null default 2000`, `updated_by uuid not null`, `updated_at`. A check constraint keeps exactly one row.

`region_failovers`: `id uuid pk`, `from_region_code text not null` fk `regions`, `to_region_code text not null` fk `regions`, `mode text not null` check in (`drain`,`promote`), `status text not null` check in (`dry_run`,`planned`,`executing`,`succeeded`,`failed`,`reverted`), `impact jsonb`, `step_log jsonb`, `started_by uuid not null`, `approved_by uuid`, `started_at`, `finished_at`, `note text`, `error text`. Index `(started_at desc)`.

`residency_migrations`: `id uuid pk`, `organization_id uuid not null` fk `organizations`, `from_region_code text not null`, `to_region_code text not null`, `status text not null` check in (`planned`,`copying`,`verifying`,`cutover`,`succeeded`,`failed`,`reverted`), `plan jsonb`, `bytes_copied bigint not null default 0`, `started_at`, `finished_at`, `error text`, `created_by uuid not null`, `created_at`. Indexes: `(organization_id, created_at desc)`, and a partial unique on `organization_id` where the status is active so one organization cannot run two migrations.

Read and write routing is configuration rather than a table: an application instance learns its region from deployment configuration, and each request's residency policy decides where its cacheable derivatives may live.

### Events

Emitted: `region.status.changed` (`{code, from, to, service?}`), `routing.policy.updated`, `region.failover.planned`, `region.failover.started`, `region.failover.completed`, `region.failover.failed`, `residency.migration.started`, `residency.migration.succeeded`, `residency.migration.failed`. Payloads carry region codes, ids and timestamps — never infrastructure addresses or credentials.

Consumed: `organization.created` to provision a residency policy (default region for both home and storage); the internal health checker's degraded signal to evaluate the failover thresholds and move a region to `degraded`.

Webhook relevance: high — enterprise customers subscribe to `region.status.changed` and the residency migration lifecycle. Delivery is region-aware: the outbound runner is pinned to the organization's home region and queues through a store that survives a regional outage, so a failover cannot cause a delivery gap; failed attempts retry with backoff after the event.

Audit: region status changes, routing policy replacements (with before/after diff), failover dry-runs and executions, and every residency-migration step, each with actor and request id.

### Acceptance criteria

- [ ] The registry lists seeded regions with status, endpoints and storage labels, and every read works on a single-region deployment with multi-region actions disabled and explained.
- [ ] Exactly one region is the default, enforced at both database and API level.
- [ ] Health checks cover all seven services per region, and a stopped service flips the region to `degraded` within the configured threshold.
- [ ] The health matrix renders history, and a region with no recent checks shows `Unknown` rather than green.
- [ ] The latency matrix shows p95 values with a timestamp, and data older than the refresh window is visibly marked as stale.
- [ ] Routing rules reject duplicate priorities, missing matches, unknown target regions and disabled targets with field-level messages.
- [ ] `Simulate` returns a region and the matched rule for country, host-suffix and path-prefix inputs, and falls back to the default region when nothing matches.
- [ ] Policy writes are atomic: a rejected replacement leaves the previous policy intact when fetched afterwards.
- [ ] A newly created organization receives a residency policy whose home and storage region are the default region.
- [ ] The residency policy rejects a storage region that is neither the home region nor a listed replica, naming the missing replica entry in the message.
- [ ] Media uploaded by an organization whose home region is `eu-frankfurt` lands in that region's bucket, and the region is recorded on the media row.
- [ ] With cross-region caching disabled, cacheable derivatives are not served from replica regions (asserted by a response-header test).
- [ ] A residency migration requires the prerequisites checklist, copies and verifies counts before cutover, and can only cut over after verification passes.
- [ ] During `copying` and `cutover` the organization is read-only and the banner is visible in the admin.
- [ ] A failed migration leaves the organization on its original region with a rollback record and no partial cutover.
- [ ] Failover dry-run returns the impact report (affected organizations, hostname changes, expected downtime estimate) and changes nothing.
- [ ] Failover execution requires the typed source-region code and a second approver when policy demands it.
- [ ] During a failover, writes to the affected region are refused with `503` plus `Retry-After`, and webhook deliveries for those organizations are queued rather than lost, then delivered after completion.
- [ ] Region status changes and policy updates appear in the audit log with actor and diff.

### QA plan

Browser walkthrough:

1. `/platform/regions` → three seeded regions with status chips; expand a row → per-service health table with latencies; the latency matrix shows values with a timestamp.
2. `/platform/regions/routing` → add a country rule for a test country pointing at `eu-frankfurt`; save an invalid rule (priority clash) → inline error and the policy is unchanged after reload.
3. Simulate a client from Turkey → resolves to `tr-ankara` and names the matched rule; simulate an unlisted country → default region; simulate a malformed country code → validation message, no crash.
4. `/settings/data-residency` → confirm the home region; save a storage region that is not a replica → rejected with the explanation; add the replica and save → accepted; the policy JSON copies cleanly.
5. Upload a media file for that organization → the region label on the media row matches the home region; with cross-region caching off, a derivative fetch from another region returns the origin-region marker.
6. `/settings/data-residency/migrate` → plan a migration to `us-virginia` → prerequisites checklist appears, the organization goes read-only with a banner during copy (verify a write is refused), verification passes, cutover completes, and the policy shows the new home region.
7. Force a verification mismatch in a test → the migration fails, the organization stays on the original region, and a rollback record exists.
8. `/platform/regions/failover` → dry-run a drain of `eu-frankfurt` → impact list renders; confirm afterwards that routing policy and region statuses are unchanged.
9. Execute a drain with typed confirmation → a write to the affected region returns `503` with `Retry-After`; a webhook delivery attempted during the window is queued and arrives after the drain completes; the step log finishes `succeeded`.
10. Restore the region → writes resume, status returns to `healthy`, and the audit log contains the status changes and the failover with actor and diff.
11. Keyboard and mobile: `g r` reaches regions and `g s` opens the simulator; at 390×844 tables are cards and routing reorder uses up/down buttons.

Visual check: status chips pair icon with label (never colour alone); latency cells show numbers with a legend; the residency badge and the dry-run impact list are legible at 1280px; destructive execute buttons are visibly destructive and the typed-confirmation helper is obvious; read-only and maintenance banners never obscure admin navigation.

### Slices

1. **Region registry + health + read surfaces.** Migration, seeded regions, the health checker with history, list/detail endpoints and screens, latency matrix.
   Done: three regions render with fresh health data, a stopped test service flips status within the threshold window, and a single-region deployment degrades gracefully.
2. **Residency policy + region-aware storage.** Per-organization policy with defaults and validation, storage writes to the home-region bucket with the region recorded, cross-region caching enforced, `/settings/data-residency`.
   Done: an uploaded object is labelled with its home region and the validator rejects an unlisted storage region.
3. **Routing policy + simulator + failover.** Policy document with rules and thresholds, whole-document writes, simulator, failover dry-run and execution with typed confirmation and queued webhook deliveries.
   Done: the simulator resolves three countries to distinct regions, a dry-run changes nothing, and an executed drain refuses writes with `503` without losing a single delivery.
4. **Residency migration + polish.** Plan/verify/cutover with the read-only banner, rollback path, status-change events, audit diffs, mobile layout.
   Done: a full migration moves a test organization between regions with verified counts, a forced failure rolls back cleanly, and the audit trail shows every step.

### Risks / notes

- Residency consistency: the policy is checked at write time (where does the object go?) and at read time (may a cache serve it?) — a policy that is only advisory is worse than none.
- Cross-region cache leakage: private or authenticated responses must never be cached at an edge outside permitted regions; keep an explicit allowlist of cacheable routes rather than a denylist.
- Replication lag: reads served from a replica can be stale; v1 keeps writes and authoritative reads in the home region and caches only derivatives, so an operator's own screens keep read-your-writes.
- DNS and proxy TTLs bound failover speed: the dry-run must state a realistic estimate instead of promising instant switching.
- Single-writer by design: document plainly that a regional outage degrades writes for affected organizations until failover completes, and that `503` plus `Retry-After` is the contract clients must honour.
- Health-check noise: threshold and de-bounce so routing does not flap on one missed check, and never auto-execute a failover without the configured approval.
- Residency migration is destructive at cutover: require a verified copy, a protected snapshot and a recorded rollback point, and keep source data until verification passes.
- Cost: object replication and per-region environments multiply storage — replica regions default to none so nothing replicates without an explicit decision.
- The registry describes infrastructure, never provisions it; otherwise a misconfigured admin action could advertise a region that does not exist.
