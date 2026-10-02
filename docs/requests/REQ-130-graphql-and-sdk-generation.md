# REQ-130 — GraphQL Surface & SDK Generation

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/content` + `packages/api-client`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Integration beyond REST.

- GraphQL endpoint over the content/tenancy/media surface with persisted queries and depth limits.
- Schema per enablement: only types the caller may read; field-level permission filtering.
- OpenAPI document generated from the REST routers; SDKs generated for TypeScript (and one more language) in CI.
- Versioned API policy: deprecation headers, changelog, sunset windows.
- Playground/explorer for authenticated admins (REQ-022 developer portal).

## Implementation spec

> **Band:** migration `0130`–`0139` reserved; append-only ledger — take the next free number if taken · **Crate:** new `crates/graphql` over existing service crates · **Package:** `packages/api-client` · **Admin routes:** `/developer/graphql/*` under the REQ-022 developer portal.

### Scope (in / out)

**In**

- GraphQL endpoint at `POST /api/v1/graphql` (session or API key, sandbox keys included), plus `GET /api/v1/graphql` for persisted-document execution with variables. Operations are not batched across unrelated requests, so cost accounting, limits and audit stay per-request.
- Resolver layer that calls the same service functions as the REST handlers: one implementation of business logic, two transports. Every resolver passes through `guards::require`, so a query can never reach data that the matching REST call could not.
- Limits: maximum query depth (default 10), per-request cost budget (default 1000 units from field weights), maximum aliases and fragments, page-size cap (100), request timeout (10 s) and separate rate-limit buckets for queries and mutations. Violations are refused before execution, with distinct error codes and no partial writes.
- Persisted documents: register a document (produced by CI from a client query file, or typed in the playground) and execute it later by `documentId` or hash with variables. A `persisted_only` setting per environment refuses ad-hoc documents in production, so a leaked read scope cannot run arbitrary queries.
- Schema per enablement: the caller's schema is composed from the modules the installation has enabled (REQ-124) and the caller's effective permissions (REQ-068). A type whose read permission the caller lacks is absent from the schema entirely — not nulled at runtime — so introspection cannot leak shape or existence.
- Field-level filtering: fields backing a narrower permission are dropped individually; mutations appear only when the corresponding write permission exists; the composed schema is cached under a key of (module set, capability set, permission-set hash, version) and invalidated on module, capability or role changes.
- Playground and explorer for authenticated admins: operation list, variables editor, per-caller schema tree, cost preview that refuses over-budget queries and names the top contributors, example gallery and copy-as-code for TypeScript and Python.
- OpenAPI emitted from the REST routers themselves at `/api/v1/openapi.json` — the single source for the API Explorer (REQ-033), the generated docs and SDK generation. A CI gate fails when a route lacks annotations or the committed snapshot drifts from the running router.
- Generated SDKs for TypeScript (`packages/api-client`, published to the package registry) and Python, both generated from a pinned OpenAPI hash per release, versioned in lockstep with the API version policy (docs/05-VERSIONING.md) and shipped with build provenance attached to the release artifacts.
- Versioned API policy: additive-only changes inside `/api/v1`; deprecated routes and fields carry `Deprecation`, `Sunset` and changelog `Link` headers; sunsets are never shorter than six months for public routes and three months for developer-internal routes; removals happen only in a major release.
- Query logging: per-operation depth, cost, duration, status and error code, retained 14 days; stored variable values are limited to registered persisted documents and never captured for ad-hoc documents.

**Out**

- A second business layer: no resolver touches tables directly and no REST handler is reimplemented for GraphQL.
- Realtime subscriptions: websocket transport stays with REQ-041 and GraphQL subscriptions follow once it lands.
- File uploads through GraphQL (multipart spec), per-query billing, SDK languages beyond the two shipped, and schema federation or third-party schema registries.
- A public playground: the explorer is authenticated and scoped to its caller, never an open endpoint.

### Screens (UI)

Routes under `apps/admin/app/(developer)/developer/graphql/*`:

| Route | Screen |
|---|---|
| `/developer/graphql` | Playground: request pane, variables, result pane, cost meter, per-user history |
| `/developer/graphql/documents` | Persisted document manager: name, hash, kind, status, hits, last used |
| `/developer/graphql/documents/{id}` | Document detail: text, operation list with cost, callers by key, revoke |
| `/developer/graphql/schema` | Schema explorer for the caller's effective schema, with role diff |
| `/developer/api/deprecations` | Deprecation list and sunset calendar with replacement links |
| `/developer/sdks` (extended) | SDK releases per language, download links, provenance, pinned document hash |

- **Playground.** Autocomplete is limited to the caller's schema; the variables editor validates JSON; the cost meter shows depth and cost before sending and blocks over-budget runs, naming the top three contributors and their weights. The result pane shows data, errors with codes, and `extensions` (depth, cost, duration, request id). A copy menu emits curl, TypeScript and Python snippets against the generated client. `Cmd+Enter` runs, `Cmd+/` toggles the snippet drawer.
- **Document manager.** Table with hash copy action, allowlist state and a revoke that warns how many callers hit the document in the last day. The create flow accepts pasted text or a file drop, validates against the caller's schema, computes the hash locally and rejects duplicates with a link to the existing row.
- **Schema explorer.** Type list grouped by domain, field list showing the permission each field requires, and a `Compare with…` action rendering the field-level diff between two roles, so the effective-schema rule is visible rather than documented.
- **Deprecations.** Columns Route or field, Deprecated in, Sunset at (countdown, amber inside 30 days), Replacement, Notified; actions Announce, Extend (reason required, audited) and a CSV export for integrator notifications.
- **States and mobile.** Loading skeletons, empty states with the create hint, error strip with retry; the playground keeps unsent text per user locally. At 390 px the panes stack vertically with a sticky run button, and tables become cards.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| POST | `/api/v1/graphql` | Execute an ad-hoc or persisted document | caller's own permissions |
| GET | `/api/v1/graphql` | Execute a persisted document by id or hash with variables | caller's own permissions |
| GET | `/api/v1/graphql/schema` | SDL for the caller's effective schema | `developer.read` |
| GET | `/api/v1/graphql/schema/diff` | Field-level diff between two roles | `developer.graphql.manage` |
| GET · POST | `/api/v1/graphql/documents` | List · register a persisted document | `developer.read` · `developer.graphql.manage` |
| GET · DELETE | `/api/v1/graphql/documents/{id}` | Detail · revoke | `developer.read` · `developer.graphql.manage` |
| GET · PUT | `/api/v1/graphql/settings` | Persisted-only mode, depth, cost budget, rate buckets, playground toggle | `developer.graphql.manage` |
| GET | `/api/v1/openapi.json` | OpenAPI document for the caller's surface | `developer.read` |
| GET | `/api/v1/api/deprecations` | Active and planned deprecations | `developer.read` |
| POST | `/api/v1/api/deprecations` | Announce a deprecation with sunset and replacement | `settings.manage` |
| POST | `/api/v1/dev/sdks/generate` | Generate an SDK archive from a pinned document hash | `developer.sdks.scaffold` |
| GET | `/api/v1/sdk/releases` | Published SDK versions per language | `developer.read` |

Execution returns `200` with a GraphQL envelope even when it contains errors; transport-level failures (`401`, `403` on the endpoint itself, `429`) keep HTTP statuses. Limit refusals use error codes (`DEPTH_LIMIT`, `COST_LIMIT`, `PERSISTED_QUERY_NOT_FOUND`, `RATE_LIMITED`) so clients branch precisely. Every route lives under `/api/v1`, and the generators consume the same document the Explorer serves.

### Data model

Migration `0130_graphql_sdk.sql` (band `0130`–`0139`; one migration for this REQ).

```sql
graphql_documents (id uuid pk, organization_id uuid -> organizations, name text, hash text, kind text default 'query' in ('query','mutation'),
  document text, operations jsonb default '[]', status text default 'draft' in ('draft','active','revoked'),
  required_for_callers bool default false, hits bigint default 0, last_used_at timestamptz, created_by uuid -> users, created_at/updated_at)
  unique (organization_id, hash), index (organization_id, status)
graphql_query_logs (id bigserial pk, organization_id uuid, api_key_id uuid null -> api_keys, actor_user_id uuid null -> users,
  document_id uuid null -> graphql_documents, operation_name text, hash text, depth int, cost numeric(10,2), aliases int,
  duration_ms int, status text in ('ok','error','rejected'), error_code text, created_at timestamptz)
  index (organization_id, created_at desc), (document_id, created_at desc)
api_deprecations (id uuid pk, route_pattern text, method text null, field_path text null, deprecated_in text,
  sunset_at timestamptz, replacement text, note text, status text default 'announced' in ('announced','active','removed','withdrawn'),
  notified_at timestamptz, created_by uuid, created_at/updated_at)  index (status, sunset_at)
sdk_releases (id uuid pk, language text in ('typescript','python'), version text, openapi_hash text, artifact_url text,
  provenance_url text, released_at timestamptz, published_by uuid null)  unique (language, version)
```

Notes: endpoint settings live in the platform settings store (REQ-112) under a `graphql.*` prefix so changes are versioned like every other setting. `graphql_query_logs` is partitioned by month and the retention job drops old partitions; variables are never stored for ad-hoc documents. The deprecation middleware reads a cached map rebuilt on write, so header overhead per request is a hash lookup.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `graphql.document.registered` · `.revoked` | Allowlist changes | `document_id`, `hash`, `name`, `actor_user_id` |
| `graphql.query.rejected` | Limit or allowlist refusal, sampled to one per key per minute | `hash`, `error_code`, `depth`, `cost` |
| `api.deprecation.announced` · `.extended` · `.sunset_reached` | Policy lifecycle | `route`, `sunset_at`, `replacement` |
| `sdk.released` | A release pipeline publishes SDK artifacts | `language`, `version`, `openapi_hash` |

Consumed: `module.enabled` · `.disabled` (REQ-124) and `plugin.enabled` · `.disabled` (REQ-121) trigger schema recomposition; `permissions.role.updated` (REQ-067) invalidates cached schemas; `licence.capability.changed` (REQ-134) invalidates when a capability adds or removes schema surface.

Webhook relevance: yes — `graphql.query.rejected` is what a security-conscious organization subscribes to (repeated refusals suggest a probing client) and `api.deprecation.announced` is how integrators learn about sunsets without reading a changelog. Payloads carry hashes and codes, never query text or variable values.

### Acceptance criteria

- [ ] A single query spanning content, media and organization types returns the same data a set of REST calls would, with the caller's permissions unchanged.
- [ ] Removing a read permission removes the corresponding type from the introspected schema, and a query naming that type fails validation instead of returning `null`.
- [ ] Depth above the limit, cost above budget, alias spam, oversized page size and over-long requests are each refused with distinct error codes and no partial execution.
- [ ] Persisted-only mode refuses arbitrary documents with `PERSISTED_QUERY_NOT_FOUND`; registered documents execute by id and by hash with variables.
- [ ] A document revoked in the UI stops executing within one cache cycle.
- [ ] Mutations exist only when the caller holds the matching write permission; a refused mutation returns `FORBIDDEN` and changes nothing in the store.
- [ ] GraphQL and REST apply identical guards: a curl pair proves equal results for equal permissions, including a shared `403` case.
- [ ] Responses carry `extensions` with depth, cost, duration and request id, verified on a real call.
- [ ] The OpenAPI document is emitted from the routers, and a CI job fails on an undocumented route or a snapshot drift.
- [ ] SDKs generate identical output from a pinned hash, and both packages compile and pass smoke tests against a live test server.
- [ ] Deprecated routes and fields return `Deprecation` and `Sunset` headers and appear in the deprecations screen and the changelog.
- [ ] A sunset in the past marks its row `removed` in the UI, and the route returns the documented final error.
- [ ] The playground refuses over-budget queries and explains the top cost contributors.
- [ ] Introspection for a read-only caller lists only fields that caller may read, verified by diffing two callers' schemas.
- [ ] Query logging records depth, cost, duration and errors; no ad-hoc variable values are stored.
- [ ] The admin screens render at 390 px without horizontal scroll and the walkthrough reports zero high findings.

### QA plan

Browser walkthrough: register a persisted document and execute it by id, then revoke it and confirm the refusal; run one query per surface; probe each limit deliberately (depth, cost, aliases, page size) and record the error codes; toggle persisted-only mode and retry an ad-hoc document; diff two roles' schemas in the explorer; announce a deprecation and verify headers with `curl -i`; generate both SDKs locally, run their smoke tests, then compare the pinned hash. Visual check: the playground panes are legible at 1280 px, the cost meter is visible before sending, the schema tree expands cleanly, deprecation badges read as text plus colour, and the run button stays reachable at 390 px.

### Slices

1. **GraphQL core behind guards.** Endpoint, resolver layer over existing services, limits, per-caller schema composition, query logging. *Done when:* acceptance 1–3, 6–8 and 14–15 pass.
2. **Persisted documents and admin surfaces.** Manager, allowlist mode, playground, schema explorer, settings. *Done when:* acceptance 4–5, 13 and 16 pass.
3. **OpenAPI and SDK pipelines.** Emission check, drift CI, TypeScript and Python generators, release artifacts with provenance. *Done when:* acceptance 9–10 pass and the drift job runs in CI.
4. **Versioned API policy.** Deprecation middleware and headers, changelog generation, deprecation screens, sunset sweeper. *Done when:* acceptance 11–12 pass.

### Risks / notes

- Parity drift between REST and GraphQL is the headline risk: one service layer, one guard chain, and a test that asserts equal outcomes for the same permission set.
- The filtered schema is per-caller and cached by permission hash; a caching bug could serve one role's schema to another, so the cache key includes the permission-set hash and tests diff two roles on every load.
- Resolvers invite N+1 access: batch loaders are mandatory and a CI performance test asserts a query-count ceiling for the heaviest list query.
- Persisted-query allowlists rot when clients change: CI registers the documents a release needs and the manager shows unused entries for pruning.
- The cost model is an approximation — weights are documented, tunable and audited; an under-priced field becomes a denial-of-service vector, so reviews of new fields must include a weight.
- Deprecations must never be silent removals: a route without a `Sunset` header is only removed in a major release, and the changelog entry is generated from the deprecation row.
- The playground is an authenticated proxy by design and stays scoped to the caller, rate-limited per user, and never stores secrets in history.
- SDKs are public artifacts: generated code must contain no environment values, hostnames or example tokens, asserted by a content scan in the release job.
