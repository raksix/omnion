# REQ-128 — Deployment Tooling (Docker, Compose, Kubernetes)

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** infra + release
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Shipping it anywhere.

- Root Dockerfile per app (api, admin, web, cli) with multi-stage builds and small runtime images.
- Compose files: developer stack (exists), small-company production stack, enterprise topology (separate DB/queue/storage).
- Helm chart with values for ingress, TLS, resources, secrets, autoscaling.
- Release pipeline producing versioned images, CLI binaries and the chart on tag.
- Upgrade documentation covering database migrations and rolling restarts.

## Implementation spec

Artifacts live where the monorepo already puts them (docs/04): `infra/docker/` for Dockerfiles, `infra/compose/` for the stack files, `infra/helm/omnion/` for the chart, `release/` for pipeline scripts and signing material, `.github/workflows/` for the pipeline itself, and `docs/deployment/` for the upgrade guides. The admin screens extend the deployment centre of REQ-024 (`/deployment/*`, permission family `deployment.*`) and read the release manifest feed it already caches; this request produces the artifacts and the screens that hand them to an operator, and it consumes the migration ordering rules of REQ-129 rather than inventing its own.

### Scope (in / out)

**In**

- **Dockerfiles.** One multi-stage file per app, root-level and build-context aware for the pnpm workspace and the Cargo workspace:
  - `api` — Cargo chef-style dependency layer, release build, distroless or scratch runtime with only the binary, CA certificates and a timezone data set.
  - `admin` and `web` — dependency install, `next build` with standalone output, node runtime on the slim base, static assets copied, non-root user.
  - `cli` — a small builder producing a stripped static binary for `amd64` and `arm64` (musl toolchain), plus the same binary inside a tiny image for CI use.
  - Every image: no build arguments carrying secrets, non-root user, read-only-rootfs friendly (only an explicit writable tmp), a `HEALTHCHECK` where the runtime supports one, OCI labels (version, revision, source, licenses), and a documented size budget — `api` under 120 MB, `admin`/`web` under 250 MB, `cli` under 40 MB compressed.
- **Compose stacks.** `docker-compose.dev.yml` stays (adds a `docker-compose.observability.yml` profile for Prometheus and Grafana over the REQ-126 metrics). New:
  - `docker-compose.prod.yml` — the small-company stack: api, admin, web, postgres, redis, minio, with named volumes, restart policies, healthchecks, a one-shot `migrate` service that runs before api starts, resource hints, and an `.env.example` listing every variable (values never in the file).
  - `docker-compose.enterprise.yml` — the same apps pointed at **external** PostgreSQL, Redis and S3-compatible storage through environment references, plus separate worker replicas; no database containers, because enterprise installs bring their own.
  - A `docker-compose.override.example.yml` showing the tuning knobs (replicas, log level, retention).
- **Helm chart.** `infra/helm/omnion/` with a deployment/service/ingress per app, a `values.schema.json` so a bad values file fails at `helm lint` time, and value groups for: image registry and tag, replicas and resources (requests and limits per component), ingress host and class, TLS via either a referenced secret or cert-manager annotations, database/redis/storage endpoints, an `existingSecret` reference for credentials, probes (`/healthz`, `/readyz` paths and timings), pod disruption budget, horizontal pod autoscaler, node selectors and tolerations, and a migration `Job` as a `pre-install`/`pre-upgrade` hook with its own retry policy. Optional sub-charts or documented external-service switches for PostgreSQL, Redis and object storage. A `NOTES.txt` prints the first steps (admin bootstrap, readyz wait) and never prints a credential.
- **Release pipeline.** On a version tag: build and push multi-arch images (`linux/amd64`, `linux/arm64`) with immutable digests, generate an SBOM per image, build the CLI binaries for Linux (gnu and musl), macOS and Windows with checksums, package the Helm chart, render the compose files for the released version, verify the image signature/attestation, and write a release manifest (version, images with digests, CLI checksums, chart version, minimum core version, migrations shipped) that the update check of REQ-024 reads. Every artifact names its source commit; nothing is published without the CI gate (tests + the REQ-129 migration verification) being green.
- **Upgrade documentation.** `docs/deployment/upgrade.md` with the ordered sequence per topology (compose, Kubernetes), the rule that migrations run before new code serves traffic, the difference between an **application rollback** (previous image tag, always possible) and a **database rollback** (only when the release shipped a verified down script, otherwise a documented restore path), post-upgrade verification steps, and a per-version notes section the panel also renders.
- **Admin screens.** Below; they generate a bundle for a named target (compose file + `.env.example`; values file for Helm), fetch release manifests, and show the upgrade path from the running version.

**Out**

- Cloud-provider IaC (Terraform modules for a specific cloud), Kubernetes cluster provisioning, load-balancer configuration outside ingress.
- Operating an operator's registry, DNS or TLS issuance (referenced, not managed).
- Windows-server container images; Windows support is the CLI binary only.
- Canary or blue-green rollout machinery — rolling update plus verified rollback is the bar (REQ-024).

### Screens (UI)

| Route | Purpose |
|---|---|
| `/deployment/artifacts` | Release artifacts for a version: images with digests, CLI binaries with checksums, chart, SBOM |
| `/deployment/artifacts/{version}` | One release: source commit, minimum core version, migrations shipped, upgrade notes link |
| `/deployment/install` | Environment bundle generator: target kind, domain, TLS mode, registry, sizes → file downloads |
| `/deployment/upgrade` | Upgrade helper: current vs target version, ordered steps, rollback split, verification checklist |

- Artifacts table columns: **Artifact · Kind · Digest/Checksum · Platforms · Size · Published**; a copy button puts the exact pull reference or checksum on the clipboard for one-click comparison on the target host. A missing artifact type renders an explicit "not published for this version" row, never a blank.
- Bundle generator: form fields — target name, kind (`compose-small`, `compose-enterprise`, `helm`), domain name, TLS mode (existing secret / cert-manager / none), image registry and tag, replica and resource presets (small/medium/large with the concrete numbers shown), optional observability profile. Output: a file list with checksums and per-file download buttons, plus the exact commands to run; the generator also offers a dry `docker compose config` and `helm template` view of what was produced. Generated files contain **references** to secrets (names, `existingSecret` keys), never values, and the screen says so.
- Upgrade helper: reads the running version and the cached manifest, lists the ordered steps for the topology, links each version's notes, shows which migrations the target ships with their destructiveness flag from REQ-129, marks the point of no return, and offers a printable checklist. Steps that need an operator decision (destructive migration present) are highlighted and must be acknowledged before the checklist renders as complete.
- States: skeleton rows; "no releases cached yet — run an update check" empty state with the action; artifacts unreachable banner with the cached timestamp; error states carry a request id. Downloads stream files with their checksum beside them.
- Keyboard: `/` search, `c` copies the focused row reference, `Esc` closes drawers. Mobile: tables become cards, the bundle form is one column, downloads stay explicit buttons.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/deployment/artifacts` | Artifacts for the cached releases, newest first | `deployment.read` |
| GET | `/api/v1/deployment/artifacts/{version}` | One release: digests, checksums, platforms, notes link | `deployment.read` |
| GET | `/api/v1/deployment/bundles` | Generated bundles for this instance | `deployment.read` |
| POST | `/api/v1/deployment/bundles` | Generate a bundle for a target (`kind`, `domain`, `tls_mode`, `registry`, presets) | `deployment.bundle.generate` |
| GET | `/api/v1/deployment/bundles/{id}` | Bundle metadata, file list, checksums | `deployment.read` |
| GET | `/api/v1/deployment/bundles/{id}/files/{name}` | Download one generated file | `deployment.read` |
| POST | `/api/v1/deployment/bundles/{id}/render` | Server-side `compose config` / `helm template` preview text | `deployment.bundle.generate` |
| GET | `/api/v1/deployment/upgrade-plan` | Ordered steps from current to target version | `deployment.read` |
| POST | `/api/v1/deployment/upgrade-plan/acknowledge` | Acknowledge a destructive-migration warning | `deployment.manage` |
| GET | `/api/v1/releases/{version}/manifest` | Release manifest (images, checksums, migrations, core minimum) | `deployment.read` |

Manifests are cached rows the update check refreshes (REQ-024 owns the fetch); every endpoint degrades to the cached copy with an honest banner. Bundle generation is a write and is rate-limited — a handful per target per hour is plenty.

### Data model

Migration: `database/migrations/0029_deployment_tooling.sql` (next free slot at tick time), additive, with a down script per the REQ-129 policy.

- `release_artifacts` — `id bigserial pk`, `version text not null`, `kind text not null check (kind in ('image','cli','chart','sbom','compose'))` `name text not null` (image reference, binary name, chart name), `digest text` (image digest or file checksum), `platforms text[] not null default '{}'`, `size_bytes bigint`, `download_url text`, `published_at timestamptz`, `manifest_version text not null`. Unique `(version, kind, name)`; index `(version, kind)`.
- `release_manifests` — `version text pk`, `channel text not null default 'stable'`, `source_commit text`, `core_min text`, `migrations text[] not null default '{}'`, `migrations_destructive boolean not null default false`, `notes_md text not null default ''`, `upgrade_notes_url text`, `fetched_at timestamptz not null default now()`, `raw jsonb not null default '{}'` (the signed manifest as received, so a digest can always be re-checked). This table is the cache the deployment centre reads when the feed is unreachable.
- `environment_bundles` — `id uuid pk`, `name text not null`, `kind text not null check (kind in ('compose-small','compose-enterprise','helm'))` `version text not null`, `config jsonb not null default '{}'` (domain, registry, presets, observability flag — never a secret value), `files jsonb not null default '[]'` (name, size, sha256 per generated file), `checksum text not null`, `generated_by uuid`, `generated_at timestamptz not null default now()`, `download_count int not null default 0`, `last_downloaded_at`. Unique `(name, version)`; index `(generated_at desc)`. Re-generating the same name and version writes a new row and keeps the previous one for comparison.
- `upgrade_plans` — `id uuid pk`, `from_version text not null`, `to_version text not null`, `topology text not null check (topology in ('compose','kubernetes'))` `steps jsonb not null default '[]'` (ordered, each with kind `backup|migrate|deploy|verify|manual`, text, command, destructiveness flag), `destructive_acknowledged_by uuid`, `destructive_acknowledged_at`, `created_by uuid`, `created_at`. Plans are derived from manifests and stored so an operator's acknowledgement is a durable fact; a plan for a newer version is regenerated, not edited.
- Indexes: `release_artifacts_version_kind_idx (version, kind)`, `environment_bundles_name_version_idx (name, version)`, `upgrade_plans_range_idx (from_version, to_version)`.

### Events

- **Emitted:** `release.artifact.published` (per artifact from the manifest fetch), `release.manifest.updated`, `deployment.bundle.generated`, `deployment.bundle.downloaded`, `deployment.upgrade_plan.created`, `deployment.upgrade_plan.acknowledged`.
- **Consumed:** `deployment.started`/`succeeded`/`failed` (REQ-024) marks the active artifact set so the panel shows what the running instance actually uses; `migration.*` events (REQ-129) fill the destructiveness flags in a plan when the manifest is silent; `observability.alert.fired` is not consumed here — alert wiring belongs to REQ-126.
- Webhook relevance: `release.manifest.updated` and `deployment.bundle.generated` are the two an operator subscribes to when a fleet of self-hosted installs is watched from one place. Payloads carry versions, digests and flags — never a file body, never a credential.
- Notification relevance: a manifest whose target version requires a below-minimum core version notifies holders of `deployment.manage` once per version through REQ-021.

### Acceptance criteria

- [ ] `database/migrations/0029_deployment_tooling.sql` applies on a fresh and a populated database, and its down script reverses it.
- [ ] Each of the four apps builds from its Dockerfile with a multi-stage strategy and produces an image within its documented size budget.
- [ ] Images run as a non-root user, start with a read-only root filesystem (documented tmp exceptions only) and pass their healthcheck.
- [ ] No image layer or build history contains a secret or a credential-shaped build argument (asserted by a scan in CI).
- [ ] `docker compose -f infra/compose/docker-compose.prod.yml up -d` boots the full small-company stack, the one-shot migrate service completes before api serves, and `/readyz` turns green without manual steps.
- [ ] The enterprise compose file starts with external PostgreSQL, Redis and S3 endpoints and contains no database container.
- [ ] `helm lint` passes with the schema present, and a deliberately wrong values file fails with a field-level message.
- [ ] The chart installs on a local cluster in CI with `--set` values for ingress host, TLS mode, resources, replicas and `existingSecret`, and every pod becomes ready.
- [ ] The migration hook runs before application pods roll, and a subsequent `helm upgrade` re-runs it in order (verified with the REQ-129 runner).
- [ ] Autoscaling values render an HPA with the configured min/max and target utilisation; a low-values install renders no HPA instead of a broken one.
- [ ] The release pipeline on a tag publishes multi-arch images with digests, CLI binaries for the documented platforms with checksums, the packaged chart and an SBOM per image.
- [ ] The pipeline refuses to publish when tests or the migration verification gate fail.
- [ ] The release manifest lists images, checksums, migrations and the minimum core version, and the deployment centre reads it.
- [ ] `/deployment/artifacts` shows digests and checksums that match the published artifacts byte for byte.
- [ ] A generated compose bundle boots on a clean host from its own files, and a generated Helm values file installs the chart unmodified.
- [ ] Generated bundles contain secret **references** only — a test greps every generated file for the fixture value and for common secret-shaped strings and finds none.
- [ ] The upgrade helper renders the ordered steps for compose and Kubernetes, splits application from database rollback, and refuses to show a complete checklist until a destructive migration is acknowledged.
- [ ] `docs/deployment/upgrade.md` covers both topologies end to end and the steps were followed verbatim during QA on the QA stack.
- [ ] `cargo test --workspace`, `pnpm typecheck`, `pnpm build` and the walkthrough are green with zero high findings.

### QA plan

The build pass runs the image builds locally and records sizes, the non-root user, the read-only filesystem run and the healthcheck result; a layer scan asserts no secret-shaped build argument. The stack pass boots `docker-compose.prod.yml` on a clean machine profile, watches the migrate service finish before api accepts traffic, opens `/readyz`, then repeats with the enterprise file pointed at fixture external services. The Kubernetes pass runs `helm lint`, a schema failure case, and a real install-then-upgrade cycle on a local cluster with the migration hook ordering asserted from job logs.

The release pass executes the pipeline in dry-run mode against a scratch registry on a fake tag, then verifies the manifest the deployment centre renders. The walkthrough visits `/deployment/artifacts`, `/deployment/artifacts/{version}`, `/deployment/install` and `/deployment/upgrade`, clicks a copy action, generates each bundle kind, downloads and diffs a file against its checksum, renders the template preview, and acknowledges a destructive-migration fixture. The visual check must see: digest columns that do not wrap mid-string, download buttons with visible checksums, the point-of-no-return marker readable without colour, readable tables at 1280 px and card layout under 640 px.

### Slices

1. **Dockerfiles + small-company stack.** Four multi-stage files, size budgets, non-root and read-only runs, `compose.prod.yml` with the migrate service, OCI labels, CI build job. *Done when:* the stack boots on a clean host and `/readyz` is green with no manual step.
2. **Enterprise topology + Helm chart.** External-service compose file, chart with schema, values groups, probes, PDB, HPA, migration hook, NOTES. *Done when:* a local-cluster install and upgrade cycle passes in CI with the hook ordering proven.
3. **Release pipeline + artifacts UI.** Tag-driven build and publish, CLI binaries, SBOM, manifest, artifact cache, `/deployment/artifacts` screens, bundle generator and downloads. *Done when:* a fake tag produces a complete manifest and the panel shows digests matching the registry.
4. **Upgrade helper + docs.** Upgrade plans, destructiveness flags from REQ-129, acknowledgement, `/deployment/upgrade`, `docs/deployment/upgrade.md` and the per-version notes workflow. *Done when:* the guide's steps are executed verbatim on the QA stack and the helper's checklist matches what the operator does.

### Risks / notes

- Image bloat creeps in one dependency at a time; the size budgets are enforced in CI with a soft warning and a hard documentation threshold, and build-time dependencies must never leak into the runtime layer.
- Multi-architecture builds are slow and expensive; cache the builder layers, publish `arm64` at release time only (not on every commit), and tag digests so a consumer can pin an exact artifact.
- The chart and the compose files drift apart unless both are checked in CI: a parity test renders both and compares probes, migration ordering, environment variable names and port numbers.
- Secrets belong to the operator's secret store or a Kubernetes Secret referenced by name; generated bundles, values files and image layers must never carry a value, and the tests above assert it rather than trusting review.
- Migration ordering is the one sequence that can corrupt a live system: the job runs before the new code serves traffic, the app image never runs migrations at boot, and the upgrade guide states the ordering for both topologies (REQ-129 owns the runner).
- A published release manifest is a contract: digests, checksums and the minimum core version are verified on download, and a mismatch is a hard failure, not a warning.
- The CLI binary on macOS and Windows is unsigned here; document the checksum-verification step plainly rather than implying notarisation.
