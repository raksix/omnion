# REQ-074 — Permission Simulator & Authorization Diagnostics

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** admin (`apps/admin`) + core (`crates/authorization`)
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Answering "why can/can't they?" without guessing.

- Simulator inputs: user, scope, resource, action; run a check.
- Allow trace: which role/group/policy granted it, and at what scope.
- Deny reason: the explicit deny or missing grant, shown in plain language.
- Diagnostics panel also used by support to explain production behaviour.
- Regression safety: simulator snapshots for critical permission combinations (tests).

## Implementation spec

> **Where:** `crates/authorization` (the one decision path of REQ-006 — this request gives it the explanation output) · `apps/api/src/routes/iam.rs` · `apps/admin/features/iam/simulator/**` (new screens) · **Migration:** `database/migrations/0020_permission_simulator.sql` (next free number at land time; the ledger is append-only — take the next free slot if one is already used) · **Admin routes:** `/settings/iam/simulator`, `/settings/iam/simulator/snapshots` · **Permission family:** `iam.simulate`, `iam.snapshots.*` · **Depends on:** REQ-006 (the guard this must mirror and the `POST /iam/simulations` route sketched there), REQ-067 (role sources), REQ-068 (catalogue keys), REQ-069 (ABAC step in the chain), REQ-070 (scope and glob steps), REQ-072 (service-account subjects), REQ-073 (expired grants and as-of time).

### Scope (in / out)

**In**

- **One function, two outputs.** `crates/authorization::decide(subject, action, resource, at)` returns the boolean the guard already uses **and** a machine-readable explanation; the guard answers from the same call. The simulator never re-implements resolution in TypeScript or in a second Rust path — a divergent copy is a release blocker.
- **Explanation chain, ordered and typed.** Steps: `authenticated` (session or key, with its scope) → `subject` (user, group memberships expanded, service account) → `bindings` (every binding considered, with source badge `direct` / `group:<name>` / `service_account`) → `roles` (with priority and inheritance path) → `permission entries` (each allow/deny entry at its scope, including the acting deny) → `scope match` (which scope level applied, and for resource scopes whether the pattern matched the path) → `policies` (ABAC rules evaluated after RBAC, with the matched condition) → `decision` (`ALLOWED` / `DENIED`, the winning source, and for a denial a plain-language sentence).
- **Inputs:** subject (user, group or service account), action (a catalogue key, chosen from the grouped catalogue), resource (site plus optional path or resource family/id), and an **as-of time** (default now) so an operator can see the state before a grant expires or after it was revoked. The result names the source of every step, never a rendered rule set.
- **Explain a real request:** a denial that reaches an operator carries a `decision_id`; the diagnostics tab resolves it and shows the same chain with the recorded inputs, the client IP and the moment — read-only, no re-evaluation inventing a different answer.
- **Plain-language denial:** a sentence an administrator can act on — *"Ahmet is denied `content.pages.update` on `acme.com` → `/legal/*` because the role `Content Reviewer` carries an explicit deny for this scope"* — plus the nearest grant that *would* allow it (the "what to change" hint), clearly labelled as a suggestion rather than a fact.
- **Snapshots for regression:** a snapshot stores inputs, the expected decision and the expected winning source, is tagged, and runs in CI against a seeded database. A drift (decision change, or the same decision from a different source) fails the suite with the diff, so an innocuous role or scope edit cannot silently widen access.
- **Coverage report:** the catalogue is crossed with the snapshot set; the report lists keys with no case, and the gate fails when any key in the privileged families (`iam.*`, `users.*`, `sites.*`, `content.pages.*`) has zero coverage.
- **Support trail:** every run is recorded (actor, inputs, verdict, duration) so a support engineer can show what they checked; the record never contains secrets and is pruned by the retention job.
- **`403` bodies get a `decision_id`** on `/api/v1` routes only, so the panel is reachable from the failure itself; the identifier reveals nothing without `iam.simulate`.

**Out (tracked elsewhere)**

- The guard itself and binding storage → REQ-006; role matrix and role versions → REQ-067; catalogue metadata → REQ-068; the ABAC editor (this request only consumes its verdicts) → REQ-069; scope and glob semantics → REQ-070; approval windows (shown through as-of time) → REQ-073; audit retention and export → REQ-039; anomaly alerting → REQ-012.

### Screens (UI)

Nav entry under the existing IAM section: **Simulator**.

| Route | Screen |
|---|---|
| `/settings/iam/simulator` | Check tab — subject / action / resource / as-of form beside a verdict card and the step chain |
| `/settings/iam/simulator?decision=…` | Explain tab — the recorded chain of one real request, read-only |
| `/settings/iam/simulator/snapshots` | Snapshot list, tags, expected verdict, coverage report and run history |

- **Check tab:** subject picker (user / group / service account), catalogue key picker grouped by category (REQ-068 grouping), resource picker (site + optional path with the glob example rendered inline), as-of picker (now / a chosen moment / "state before this grant expired"). `Run` is `⌘↵`; the verdict card is the loudest element on the page: `ALLOWED` or `DENIED` in a labelled chip with the winning source beneath it, and a `Copy as test case` action that produces a ready snapshot payload.
- **Step chain** renders each step as a collapsible row with an icon, a one-line summary and the raw detail on expand; allowed steps are neutral, the deciding step is highlighted, and steps that did not apply say *"not evaluated — decision already made"* rather than being hidden. A `Show only the deciding path` toggle collapses noise without pretending the other steps did not run.
- **Deny panel** carries the sentence, the winning source (role, group, policy or *no grant*), the suggestion block (`What would allow this`) and a link to the exact role or binding the operator would change — with the link opening that screen rather than performing the change.
- **Snapshots tab** columns `Name`, `Subject`, `Action`, `Resource`, `Expected`, `Tags`, `Last result`, `Updated`; row actions run, edit, duplicate, delete; a run-history drawer shows per-run pass/fail and the diff for a failing case. The coverage band shows `keys covered / total` with the uncovered list one click away.
- **States, keyboard, mobile:** `EmptyState` before the first run (*"Pick a subject and an action"*), skeleton while resolving, a clear error state when the subject or key is unknown; `⌘↵` runs, `/` focuses the subject picker, `?` opens the shortcut sheet; below `lg` the chain becomes a vertical stepper, the verdict card stays sticky at the top, and the form is single-column.
- **No invented results:** the panel renders only what the API returned; when the endpoint fails the screen shows the failure, never a stale verdict from a previous run.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| POST | `/api/v1/iam/simulations` | Run a check; returns the verdict, the step chain and `run_id` | `iam.simulate` |
| GET | `/api/v1/iam/simulations/{id}` | Replay one recorded run (same inputs, same as-of time) | `iam.simulate` |
| POST | `/api/v1/iam/simulations/explain` | Resolve a `decision_id` from a real 403 into its chain | `iam.simulate` |
| POST | `/api/v1/iam/simulations/diff` | Compare two subjects, or one subject before/after a window | `iam.simulate` |
| GET, POST | `/api/v1/iam/snapshots` | List with tags and last result / create from explicit inputs or a copied run | `iam.snapshots.read` / `iam.snapshots.manage` |
| GET, PUT, DELETE | `/api/v1/iam/snapshots/{id}` | Detail with history / edit expected verdict and tags / delete | `iam.snapshots.read` / `iam.snapshots.manage` |
| POST | `/api/v1/iam/snapshots/run` | Run a set (all, by tag, or a list); per-case pass/fail and diff | `iam.snapshots.manage` |
| GET | `/api/v1/iam/snapshots/coverage` | Keys with and without coverage, grouped by category | `iam.snapshots.read` |

Every route carries `guards::require("<key>")`; responses are `no-store` because a chain describes a security configuration, and a subject from another organization answers `404`, never a partial chain.

### Data model

```text
simulator_snapshots      (id, organization_id nullable (null = platform case), name, tags text[],
                          subject_type check (subject_type in ('user','group','service_account')),
                          subject_ref text,                       -- email slug or group key, not an fk: cases must survive seed resets
                          action_key text not null references permissions (key) on delete cascade,
                          scope_type text, site_ref text, resource_family text, path_pattern text,
                          expected_decision boolean not null, expected_source text,
                          as_of_days_offset integer, created_by uuid, created_at, updated_at)
simulator_snapshot_runs  (id, snapshot_count integer, passed integer, failed integer, started_at, finished_at,
                          trigger check (trigger in ('manual','ci')), actor_id uuid, detail jsonb)
simulator_runs           (id, organization_id nullable, actor_id uuid, subject_type, subject_ref, action_key,
                          scope_type text, site_ref text, path text, as_of timestamptz,
                          decision boolean not null, source text, steps jsonb, duration_ms integer, created_at)
```

Indexes: `simulator_snapshots_action_idx (action_key)`, `simulator_snapshots_tags_idx` (GIN on `tags`), `simulator_snapshots_org_idx (organization_id) where organization_id is not null`, `simulator_runs_actor_idx (actor_id, created_at desc)`, `simulator_runs_created_idx (created_at)` for the retention prune. `simulator_runs.steps` is stored so `GET /simulations/{id}` replays without recomputation.

Migration `database/migrations/0020_permission_simulator.sql`: creates the three tables, comments in the `0002` style, seeds the platform snapshot set (a curated list of the highest-risk combinations: last-owner removal attempts, resource-scope denies, expired-grant boundaries, service-account key narrowing) as rows with `organization_id = null`, and adds nothing else.

### Events

- Emitted: `iam.simulation_run` (subject, action, decision, duration — sampled, since a support session can run dozens), `iam.snapshot_created`, `iam.snapshot_updated`, `iam.snapshot_failed` (with the case name and the diff summary), `iam.snapshot_coverage_regressed`, `iam.decision_explained` (only through the explain path, carrying the `decision_id`).
- Consumed: `iam.role_permissions_changed`, `iam.binding_created`, `iam.binding_revoked`, `iam.resource_grant_changed` and `iam.policy_changed` mark the affected snapshot set as *suspect* so the next CI run highlights which changes moved a verdict. Nothing in this request changes access — it only observes it.
- Payloads carry the case name, ids and the decision — never a full chain and never a secret; the security centre (REQ-012) subscribes to `iam.snapshot_failed` because a moved verdict can mean an unintended widening.

### Acceptance criteria

- [ ] `0020_permission_simulator.sql` applies on a fresh and on a populated database and seeds the platform snapshot set; `cargo test --workspace` is green.
- [ ] The simulator verdict equals the guard verdict for every case in a seeded matrix of ≥ 100 (subject, action, resource, as-of) combinations, asserted as one property test that calls both paths.
- [ ] No second implementation exists: a static check proves the TypeScript panel contains no permission-resolution logic (only rendering of API output).
- [ ] The chain order is exactly authentication → subject → bindings → roles → entries → scope → policies → decision, asserted structurally in a test, and steps that did not run are marked as such.
- [ ] An allow names the exact role and scope that granted it; a deny names the winning explicit deny, and when the reason is "no grant" the panel says so in plain language.
- [ ] A group-derived grant shows the group by name and the binding behind it; a grant held through a service account shows the key prefix, never the secret.
- [ ] A resource-scoped case shows whether the glob matched the path, with the pattern rendered next to the example path.
- [ ] An expired temporary grant (REQ-073) with an as-of time before expiry answers `ALLOWED` and after expiry answers `DENIED`, and the chain shows the expiry step.
- [ ] The ABAC step appears only after the RBAC steps and names the matched condition when it decides; a case with no policies says *"no policy matched"* rather than omitting the step.
- [ ] A real `403` carrying a `decision_id` resolves through `POST /simulations/explain` into the same chain, and an unknown or expired id answers `404` with a clear message.
- [ ] The "what would allow this" hint is computed from the same resolution path and is labelled as a suggestion; it never claims a change was made.
- [ ] Snapshot runs fail on a drifted decision **and** on a changed winning source, printing the affected case name and the before/after; running the same set twice with no changes reports the same result (deterministic order).
- [ ] Coverage reports every privileged-family key with zero cases, and the CI gate fails when one exists; the uncovered list is reachable from the screen.
- [ ] `simulator_runs` records actor, inputs, verdict and duration for every run, contains no secret, and is pruned by retention; a run by an actor without `iam.simulate` is impossible (`403`).
- [ ] The panel renders only returned data: a failing endpoint surfaces the error, never a previous verdict; unknown key, unknown subject and foreign organization each render their own message, and no chain leaks a foreign subject.
- [ ] A run against a seeded 5k-binding organization answers in under 300 ms at p95, and the panel has empty, loading and error states with zero high findings in the QA pass.

### QA plan

Extend `scripts/qa/walkthrough.cjs` with `/settings/iam/simulator` and `/settings/iam/simulator/snapshots` (desktop) plus `/settings/iam/simulator` (mobile). The script must run one case expected `ALLOWED` and one expected `DENIED` against the seeded fixture, assert the verdict card text and that the deciding step is highlighted, expand the deny panel and assert the plain-language sentence plus the suggestion block are present (and that clicking the suggestion navigates without changing anything), copy a run as a test case and save it as a snapshot, then run the snapshot set and assert the pass/fail summary; finally it must set an as-of time before an expired grant and assert the verdict flips. Screenshots `page-iam-simulator`, `page-iam-simulator-denied`, `page-iam-simulator-snapshots`, `mobile-iam-simulator`; the visual check looks for an unmistakable verdict chip at AA contrast, a readable chain with no clipped JSON and a coverage band that states the numbers honestly.

### Slices

1. **Chain and check tab.** `crates/authorization::decide` returning the explanation, `POST /simulations` plus replay, the form, the verdict card, the step chain and the deny panel with its suggestion hint. *Done when:* acceptance 1–5 and 16 pass and the parity property test is green.
2. **Context, resources and the explain path.** As-of time, group/service-account sources, resource glob step, ABAC step ordering, `decision_id` on `403` bodies, `POST /simulations/explain`, `POST /simulations/diff`, run records. *Done when:* acceptance 6–11 and 15 pass and a real 403 resolves into a chain in the walkthrough.
3. **Snapshots, coverage and CI.** Snapshot CRUD from a copied run, platform seed set, deterministic run set with drift diffs, coverage report and gate, suspect-marking on change events, retention prune. *Done when:* acceptance 12–14 pass and a deliberately drifted expectation fails CI with a readable diff.

### Risks / notes

- **The simulator must not become a second authority.** If its verdict and the guard's ever disagree, the wrong one wins trust; the parity test is the guard rail, and any performance shortcut inside the simulator that skips a step is a defect, not an optimisation.
- **Explanations expose structure.** A chain reveals role names, scopes and patterns — exactly what an attacker wants to map. `iam.simulate` is a privileged key, responses are `no-store`, run records are pruned, and a chain for a foreign organization is a `404`.
- **Snapshot maintenance is a real cost.** Expected values move when a role legitimately changes; the drift diff must make the reason obvious, and the run output should point at the change event that made a case suspect rather than letting an operator "fix" the expectation blindly.
- **As-of time is a query, not a history.** It reconstructs state from `created_at`/`revoked_at`/`expires_at` on live rows; it cannot answer for rows that were hard-deleted, so deletion paths keep the append-only rule of REQ-006.
- **`decision_id` in 403 bodies** must be opaque, short-lived and useless without the key; if it ever encodes the inputs itself, it becomes a leak channel on the public surface.
- **Language discipline.** The chain is read by support engineers under pressure: one sentence per step, no jargon soup, and the suggestion block must never read as if the panel had changed anything.
