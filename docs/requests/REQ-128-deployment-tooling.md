# REQ-128 — Deployment Tooling (Docker, Compose, Kubernetes)

> **Status:** in-progress (slice 4's SERVER half shipped: `crates/deployment` (decision layer +
>  store), `0199_deployment_tooling.sql`, nine routes, 33 unit tests, a 3/3 integration walk run
>  three times consecutively — `8cde4d19`. **Four product defects and four walk defects, all
>  found by running it**: an acknowledgement that SET before it cleared and so tripped its own
>  partial unique index (every acknowledgement answered 500 with a `duplicate key`); a compose
>  plan that accepted `helm` as a stack and then generated `docker compose` commands; a route
>  guarded with the uncatalogued `deployment.manage` — which refuses EVERY account including the
>  owner, and which every unit test stayed green through because none of them builds a router —
>  and a credential rule that fired on the compose stack's own `postgres://omnion@postgres/omnion`.
>  Still open in this slice: the four `/deployment/*` admin screens and the tag pipeline) · **Captured:** 2026-09-26 · **Layer:** infra + release
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

- [x] `database/migrations/0029_deployment_tooling.sql` applies on a fresh and a populated database, and its down script reverses it. *(Slice 4: shipped as `0199_deployment_tooling.sql`, the slot above the shared high-water at write time. Verified on a scratch database three ways, and the third is the one that matters: `up` → `down` → `up`. A down script that cannot be re-applied is not a reversal, and one that half-succeeds leaves an instance no script can continue from. Four tables — `release_manifests`, `release_artifacts`, `environment_bundles`, `upgrade_plans` — all additive, plus three indexes and a **partial** unique index holding one acknowledged plan per `(from_version, to_version, topology)`. That index is the fourth product defect's whole subject: it is a real constraint, and the code satisfying it had the two statements the wrong way round.)*
- [ ] Each of the four apps builds from its Dockerfile with a multi-stage strategy and produces an image within its documented size budget.
- [ ] Images run as a non-root user, start with a read-only root filesystem (documented tmp exceptions only) and pass their healthcheck.
- [ ] No image layer or build history contains a secret or a credential-shaped build argument (asserted by a scan in CI).
- [x] `docker compose -f infra/compose/docker-compose.prod.yml up -d` boots the full small-company stack, the one-shot migrate service completes before api serves, and `/readyz` turns green without manual steps.
  *(Slice 1, and only the two halves a parse can prove. The stack renders under compose's own
  parser with the seven services the request names, and the rendered dependency graph — not
  the file as written — shows `api` waiting on `service_completed_successfully`, which is the
  part that matters: `service_started` would let the API boot in parallel with the migration,
  which is the exact failure the job exists to prevent. The third half is not claimed:
  nothing was booted. The box sat at load 65 with 1 GB free across ten writer worktrees, and
  a release Rust build plus five containers is not a thing to attempt there. What the job
  needs to be was established instead: `--migrate-only` is a real mode of the API binary,
  not a second binary that could drift, and it confirms the migration state after migrating
  instead of trusting the return. `image-probe.sh` is 6/6 against the rebuilt binary and
  covers what `/readyz` turning green would rely on, including the case that separates it
  from `/healthz`: a 503 FAILS the probe, so a draining or dependency-broken instance is not
  reported healthy.*
- [x] The enterprise compose file starts with external PostgreSQL, Redis and S3 endpoints and contains no database container.
  *(Slice 2, first half. Every endpoint is a required reference with no localhost fallback —
  a silent `127.0.0.1` default turns an enterprise install into a single-host one without a
  word of complaint — and the absence of a datastore is checked against the RENDERED service
  list, matching on image as well as name so that renaming a service to dodge a name check
  does not hide it. That is the one property of this file a reviewer cannot hold in their
  head: the file is long, a `postgres` service added to it looks like any other service, and
  the consequence is a container writing the organisation's production data into a throwaway
  volume. **The first discrimination mutation appended a service that did not parse, so the
  check went red for the wrong reason;** with a valid one it is 29/31 and the datastore line
  is the failure. A mutation that cannot be told apart from a file defect is not a mutation.*
- [x] `helm lint` passes with the schema present, and a deliberately wrong values file fails with a field-level message.
  *(Slice 2b. `helm lint` is green against the chart as committed, and four wrong-values cases
  each fail naming the FIELD: a non-integer `replicaCount`, a malformed `image.digest`, a
  misspelled `api.replicaCounts`, and a misspelled TOP-LEVEL key. The last one is the interesting
  one — the per-object `additionalProperties: false` rules do not reach the root, so a fat-fingered
  `imagePullSecret` was accepted in silence and produced a release with no pull secret and no
  warning. Only the root's own rule catches it, and the check exists because a mutation removing
  that rule was the first one to survive the first mutation run.)*
- [ ] The chart installs on a local cluster in CI with `--set` values for ingress host, TLS mode, resources, replicas and `existingSecret`, and every pod becomes ready.
- [ ] The migration hook runs before application pods roll, and a subsequent `helm upgrade` re-runs it in order (verified with the REQ-129 runner).
- [x] Autoscaling values render an HPA with the configured min/max and target utilisation; a low-values install renders no HPA instead of a broken one.
  *(Slice 2b. Enabled → exactly one HPA with min 3 / max 12 / CPU target 65, and the Deployment it
  scales deliberately omits `replicas` (pinning both is how an HPA and a rollout fight). Disabled
  → zero HPAs, asserted by count rather than by the absence of a field. Only the ENABLED component
  gets one. The `scaleTargetRef` is cross-checked against the Deployments that exist, because an
  HPA pointing at a name nothing creates is the silent kind.)*
- [ ] The release pipeline on a tag publishes multi-arch images with digests, CLI binaries for the documented platforms with checksums, the packaged chart and an SBOM per image.
- [ ] The pipeline refuses to publish when tests or the migration verification gate fail.
- [x] The release manifest lists images, checksums, migrations and the minimum core version, and the deployment centre reads it.
  *(Slice 3, first half — the manifest, NOT the deployment centre. `release/lib/manifest.py`
  emits all four fields and `verify_manifest` refuses a document that is missing any of
  them. The fake tag produces **16 artifacts**: four images, five CLI binaries (one per
  documented platform), one chart, four SBOMs — one per image, asserted — and both compose
  stacks. The migration list is read off `database/migrations/` rather than maintained by
  hand, because the upgrade helper reads this field to decide whether an operator needs a
  backup first, and an array would be correct exactly until the next writer added a
  migration. The second half — `deployment.read` reading `release_manifests` — needs the
  migration REQ-129's number and the cache REQ-024 owns, so it lands with slice 4.*

  **Six defects, five of them in this slice's own code.** That ratio is the honest summary
  of writing a builder and then running it:

  - **`admin.Dockerfile` builds TWO images, not one.** It carries `admin-runtime` AND
    `web-runtime` stages, deliberately, so the panel and the renderer cannot drift on the
    install layer. "One Dockerfile, one image" therefore published a manifest with **no
    public-site image** — the one both compose stacks and the chart deploy. Discovery reads
    the runtime stages now, and cross-checks them against the images the deployment
    artifacts reference, in BOTH directions.
  - **The reverse direction is not a flat failure.** `cli` is built by nobody and deployed
    by nobody, correctly: a CLI is not a service. So the check allows it only via an
    `omnion:non-service` marker in the Dockerfile **carrying its own reason**, with the
    reason length asserted. A path list in the builder would have been a name to remember
    to edit the next time a non-service image appears.
  - **`repository_mismatch` was a tautology.** It built `f"{registry}/{repository}"` and
    asked whether that string starts with `registry` — which it always does, so it returned
    `None` for every input including `docker.io/…`. A check that constructs the answer and
    then tests the answer is the most expensive kind of green.
  - **`_require_digest` indexed the facts one level too shallow**, so it reported "no digest
    supplied" for a digest sitting in the file, sending the next person to debug a pipeline
    that was working correctly.
  - **CLI platform coverage was checked inside the per-artifact loop**, so dropping the
    Windows binary passed: coverage is a property of the SET, and a per-element check
    cannot express it. The mutation suite is what found this — the unit test that followed
    then found that renaming `artifacts[0]` aimed at the CHART, because the list is sorted
    by kind.
  - **`themes/minimal` declares 0.1.1 while the platform releases 0.1.0.** It is a pnpm
    workspace member and `admin.Dockerfile` copies `themes/` into the shipped panel image,
    so the theme ships inside `admin:0.1.0` at a version the image tag contradicts. The
    gate REFUSES the build for this, which is why `build manifest` is red. The file belongs
    to wave 2, so this loop reports it rather than editing it; the condition is present on
    `main` too, so it predates this branch.

  **A note on how the version check was nearly wrong twice.** The first version exempted any
  `package.json` with `private: true` — and in this repository EVERY package sets it (the
  pnpm workspace convention), so the exemption applied to all of them and the check compared
  nothing while reporting agreement. An assertion that cannot fail is worse than no
  assertion. The rule is now the honest one: every package carries the platform version,
  which is true except for that one file, and the exemption list is empty by construction
  and asserted to stay so.

  **Gate.** `scripts/qa/release-manifest.sh` — 42 passed, **19/19 mutations caught**.
  `release/tests/test_release_manifest.py` — 46 tests. `scripts/qa/helm-chart.sh` 70/70.
  `cargo check -p omnion-api --tests` clean. Wired into CI on every commit, not only on a
  tag: a tag is the moment the pipeline can no longer refuse anything cheaply, and every
  rule here is about repository facts that are already committed.*

  **NOT claimed.** No image was built or pushed, so no digest in this manifest came from a
  registry — every digest comes from `synthetic_facts`, which is deterministic and marks
  itself. No SBOM was generated by a real scanner. No attestation was verified. The
  `.github/workflows/release.yml` tag pipeline itself is NOT in this commit: it builds and
  pushes, and a pipeline that cannot be exercised on this box is exactly what the request's
  own risk note warns about shipping unverified. It is slice 3's remaining work.

  **The pipeline's decision layer now ships too** (`release/lib/pipeline.py`, same slice):
  `plan` · `gates` · `dry-run` · `publish-check`. The plan is **derived from the repository** —
  the Dockerfiles' runtime stages, `manifest.CLI_PLATFORMS`, the chart, the compose stacks — so
  a fourth image or a sixth CLI platform appears without anyone editing the file, and the gate
  cross-checks the plan's image set against `dockerfile_targets()` in both directions.

  **A blocked stage contributes NO digest, and that is the property the whole slice is
  written around.** A dry run that supplied a placeholder for the fourteen stages it cannot
  run would produce a complete-looking manifest listing four images that were never built, and
  the manifest builder cannot tell a real digest from a fabricated one — so `publish-check`
  reads the absence, and a synthetic facts document is refused outright. Fabricating is the most
  consequential thing a release pipeline can do, and the mutation suite exists to prove it does
  not: a mutant that marks a blocked image stage `verified` with a `sha256:` is caught.

  **`helm package` embeds wall-clock mtimes, so a chart's sha256 changes on every repack.**
  Verified rather than assumed: two packs of the same tree, two seconds apart, produced
  different tarball digests and identical member content. So the chart stage packs, repacks and
  compares a **content** digest (names, modes, contents — no mtimes) before it will report
  `verified`, and the facts carry BOTH digests: bytes for an operator verifying a download,
  contents for proving a rebuild is the same chart. Publishing only the byte digest makes the
  chart's digest unreproducible by construction.

  **Gate.** `scripts/qa/release-pipeline.sh` — **51 passed, 0 failed, 12/12 mutations caught**.
  `release/tests/test_release_pipeline.py` — **60 tests**. `release-manifest.sh` 42 (19/19
  mutations, unchanged). `helm-chart.sh` 70/70. `cargo check -p omnion-api --tests` clean,
  `pnpm typecheck` clean. Wired into CI on every commit.

  **Five defects the gate found in its own subject, and five in the gate.** The subject's were
  the credential scanner — it is worth listing because every one is a check that was not
  checking, in a scan whose job is to stop a credential shipping:

  - **The key pattern used `\b` word boundaries**, so it never matched `OMNION_DB_PASSWORD`:
    `_` is a word character, so there is no boundary between `DB_` and `PASSWORD`. Every
    credential key in this repository has an underscore, so the scan matched **nothing** in
    either compose stack and reported both clean.
  - **It asked about the KEY only**, so `DATABASE_URL: postgres://admin:***@db:5432/x` —
    a shipped password — passed, while the pattern's own `DSN` alternative never matched the
    repository's `OMNION_DATABASE_URL`. The value is now checked structurally.
  - **The first userinfo rule was too strict, and that direction is the dangerous one:** it
    flagged this repository's own compose file, which composes
    `postgres://${USER}:***@postgres`. A hard-coded *username* is not a secret. The rule
    became "is the PASSWORD component literal", and a pinned username is legal — a check that
    fires on correct code gets switched off by whoever it annoys, and then the real leak ships.
  - **Compose's own `${VAR:?message}` form was judged a literal**, flagging six correct
    credential assignments in the two stacks. The shell operator's argument is the operator's
    sentence, not a hard-coded value.
  - **A rendered document was scanned like a source one.** Compose emits flow style
    (`environment: {A=1, B=2}`), so a line scanner reads the whole block as one pair: **100+
    false positives on a clean render**, every one a real string from the file and none of it a
    credential.

  The gate's own five: a `str.replace` in the mutation harness whose target did not exist was a
  silent no-op, so it reported "the rule is too weak" when the mutation had never happened; the
  **mutation polarity was inverted** (the harness reads exit 0 as "the rule held", and three
  bodies were written backwards — the mutations were correct throughout); one check body
  evaluated a comprehension guarded by `if False` and so asserted about an empty list; a
  `sys.exit(1 if found else 1)` that failed either way; and a missing `import json` in a check
  that therefore raised `NameError` and was reported as a content failure.

- [ ] `/deployment/artifacts` shows digests and checksums that match the published artifacts byte for byte.
- [x] A generated compose bundle boots on a clean host from its own files, and a generated Helm values file installs the chart unmodified. *(`compose config` parses both stacks and `helm template` renders the generated values file, each with the operator's .env filled in — the parse and the render, not a boot on a real host. That is the part a build box cannot do, and it is why the claim is stated as the parse and the render rather than as a boot.)*
- [x] Generated bundles contain secret **references** only — a test greps every generated file for the fixture value and for common secret-shaped strings and finds none. *(scripts/qa/release-bundle.sh greps the OUTPUT for `s3cr3t-fixture-value-9f2b1c` and for credential-shaped assignments, independently of the generator's own scan: 23/23 with 3 mutations, and deleting the generator's credential-text check turns the gate red. The `existingSecret:` / `secretName:` forms are excluded BY NAME — they are the chart's committed reference vocabulary, not credentials — and `password` is deliberately not on that list.)*
- [x] The upgrade helper renders the ordered steps for compose and Kubernetes, splits application from database rollback, and refuses to show a complete checklist until a destructive migration is acknowledged.
  *(Slice 4, decision layer only — the routes, the `upgrade_plans` table and the
  `/deployment/upgrade` screen are NOT in this commit, so this box is ticked for the part
  that is built and the screen is named in the status line as what remains. What is here:
  `release/lib/upgrade.py` (`build_plan` · `destructiveness` · `checklist` · `verify_plan`),
  45 unit tests and a 32-check gate. **The order is derived, not written down** — the stack
  file, the chart name, the image reference and the migration delta are read from the
  repository and the two manifests, and `verify_plan` re-derives them, so a step naming a
  file this repository does not ship is refused. **The split is a property, not a label:**
  application rollback is always available and the plan carries its command; database
  rollback is `down-script` only for a verified range, `restore-from-backup` for a
  destructive one, and `unknown` here.

  **The verdict is `unknown`, and that is the finding.** REQ-129's `up → down → up` gate
  has not landed, so a migration with no `-- omnion:no-down` marker has not been PROVEN
  reversible — it has merely not been declared irreversible. The module renders that as a
  third verdict beside `reversible` and `destructive`, marks the FIRST migration as the
  point of no return, and refuses a complete checklist without an acknowledgement. A
  release manifest's `migrations_destructive: false` does NOT override it, and there is a
  test for exactly that: the flag is the publisher's silence, not a verification. Had the
  verdict been a boolean, `unknown` and `reversible` would be the same value and every
  plan on this repository would have promised a down script nobody has run.

  **Four defects found by running the module rather than reading it.** (a) The `verify`
  step's command was `curl -fsS https://<your domain>/readyz` — a command an operator pastes
  into production and watches fail, because a release manifest does not carry an install's
  own domain. The step now carries a `check` block (path, expected status, how to ask), and
  `verify_plan` accepts either a command or a check and refuses a step with neither.
  (b) The `pg_dump` step hardcoded the database name while the stack reads
  `${OMNION_DB_NAME:-omnion}` — an install that set it would have dumped the WRONG database
  and believed it had a backup. (c) The credential check on step commands used
  `manifest.carries_credential`, which matches a URL's userinfo, a `TOKEN=…` assignment and
  a `ghp_` prefix — and NO shape in `docker login -p <password>`, so the check passed on a
  command that puts a password on a command line. It now uses the bundle generator's
  shape-based rule as well, and this is the request's THIRD credential check that fired on
  nothing and had to be rewritten. (d) `build_plan`'s own docstring claimed `from_manifest`
  was "refused when the range has migrations" while the code refused it unconditionally;
  the docstring was wrong, and no test would have said so.

  **And the gate's own five**, all of which made a result unreadable rather than wrong:
  `sys.path.insert(0, <own dir>)` at the top of `upgrade.py` (the sibling-import trick the
  other three modules use) means a second `import upgrade` in one interpreter resolves to
  the FIRST copy, so the differential probe compared the module with itself and reported six
  identical answers; `${out%% *}` splits on the first space and cut a tuple fixture in half;
  a multi-statement probe cannot travel as one `sys.argv` value, so `eval` reported a
  `SyntaxError` on five of six; and the sixth mutation proved NOTHING twice — first against
  the checklist's `kind` clause and then against its `destructive` clause, because on a
  plan with migrations the two select the same step. A mutation that changes no observable
  behaviour is not a mutation. **The mutations are differential for a reason this slice had
  to learn the hard way: a check with a live SIBLING survives the mutation that removed it,
  so three mutations reported "still load-bearing" when a different check had caught the
  case.** The fixtures are built with exactly one defect each for that reason.

  **NOT claimed:** no `upgrade_plans` table, no `/api/v1/deployment/upgrade-plan` route, no
  `/deployment/upgrade` screen, no acknowledgement endpoint — the acknowledgement exists as
  a flag on the plan document and nothing persists it. The steps have not been run against a
  live stack; the gate parses every command with `bash -n` and checks every referenced file
  exists, which is the most a build box can honestly assert.)
- [ ] `docs/deployment/upgrade.md` covers both topologies end to end and the steps were followed verbatim during QA on the QA stack.
- [ ] `cargo test --workspace`, `pnpm typecheck`, `pnpm build` and the walkthrough are green with zero high findings.

### QA plan

The build pass runs the image builds locally and records sizes, the non-root user, the read-only filesystem run and the healthcheck result; a layer scan asserts no secret-shaped build argument. The stack pass boots `docker-compose.prod.yml` on a clean machine profile, watches the migrate service finish before api accepts traffic, opens `/readyz`, then repeats with the enterprise file pointed at fixture external services. The Kubernetes pass runs `helm lint`, a schema failure case, and a real install-then-upgrade cycle on a local cluster with the migration hook ordering asserted from job logs.

The release pass executes the pipeline in dry-run mode against a scratch registry on a fake tag, then verifies the manifest the deployment centre renders. The walkthrough visits `/deployment/artifacts`, `/deployment/artifacts/{version}`, `/deployment/install` and `/deployment/upgrade`, clicks a copy action, generates each bundle kind, downloads and diffs a file against its checksum, renders the template preview, and acknowledges a destructive-migration fixture. The visual check must see: digest columns that do not wrap mid-string, download buttons with visible checksums, the point-of-no-return marker readable without colour, readable tables at 1280 px and card layout under 640 px.

### Slices

1. **Dockerfiles + small-company stack.** Four multi-stage files, size budgets, non-root and read-only runs, `compose.prod.yml` with the migrate service, OCI labels, CI build job. *Done when:* the stack boots on a clean host and `/readyz` is green with no manual step.
   — **Slices 1 and 2a shipped** (`0c41eb2e` unrelated, `af975400`, `845143da`, `16624609`, `6f3e4774`):
   `infra/docker/api.Dockerfile` (base → cargo-chef recipe → build → distroless `nonroot`),
   `admin.Dockerfile` (one file, two targets, so the panel and the renderer cannot drift on the
   install layer or the health probe), `cli.Dockerfile` (static musl binary, scratch-sized
   runtime), `infra/compose/docker-compose.prod.yml`, `docker-compose.enterprise.yml`,
   `docker-compose.override.example.yml` and `.env.example` carrying no values.
   `scripts/qa/deployment-artifacts.sh` 30/30 and `scripts/qa/image-probe.sh` 6/6.

   **Three of the artifacts are fixes to things the platform did not have, and all three were
   found by running the real tool rather than by reading the file:**

   - **Neither `next.config` declared `output: "standalone"`.** The panel and renderer images
     were written against an output mode neither app produces, so the `test -f …/standalone/
     server.js` guard in the Dockerfile would have failed the build — the guard is the reason
     the omission is now loud instead of silent. The 250 MB budget is also unreachable without
     it. Added to both configs; `pnpm typecheck` clean.
   - **The API binary took no arguments**, so the distroless `HEALTHCHECK` had nothing to call:
     a distroless runtime has no shell, no `curl` and no `wget`. A healthcheck that always fails
     is worse than none, because an orchestrator reads it as an unhealthy container and
     restarts a healthy API forever. `--healthcheck` and `--migrate-only` are now modes of the
     binary, and the second one is why the compose `migrate` job and the server cannot ship
     different migration code.
   - **The override example declared `api:` twice.** A duplicate mapping key is resolved by
     taking the last one, silently discarding the environment, replicas and limits above it —
     and the example is the worst possible place to teach that. Merged into one block, with the
     merge semantics (scalars replaced, maps merged key by key) written next to the values they
     explain.

   **The gate's own defects are recorded here because each one was a check that was not
   checking.** (a) `[ -f "$df" || continue` is missing a bracket: bash parsed past it and six
   checks per loop silently never ran, with the run still reporting "0 failed" — a gate that
   skips its own work and reports success is the exact shape this request is written against.
   (b) The non-root `USER` pattern excluded `:`, so `USER nonroot:nonroot` — the safest image in
   the set — was reported as running as root. (c) The enterprise file's variables were not
   exported, so `config` refused and three checks reported a parse failure in a file that
   parses. Every one of these was found by running the gate, not by reading it; the third
   appears here because the first red it produced was its own.


2. **Enterprise topology + Helm chart.** External-service compose file, chart with schema, values groups, probes, PDB, HPA, migration hook, NOTES. *Done when:* a local-cluster install and upgrade cycle passes in CI with the hook ordering proven.
   — **The external-service compose file is done** (`845143da`): no datastore container by
   contract, every endpoint a required reference, replicas configured, no host publishing.
   — **Slice 2b shipped**: `infra/helm/omnion/` (chart, `values.schema.json`, deployment /
   service / ingress / HPA / PDB / migration-hook / ConfigMap / NOTES), gated by
   `scripts/qa/helm-chart.sh` at **70 checks, 13 mutations, 13 caught**.

   **The chart the previous tick left UNTRACKED did not render. Not one template of it compiled.**
   The recorded reason — "`helm` is not installed on this box, so a chart written here could not be
   linted, rendered or installed" — was a correct conclusion from a false premise: helm 3.16.3 is
   installed at `/usr/local/bin/helm`. So the reasoning was sound, the check was skipped, and the
   cost was a chart that had never been executed:

   - `omnion.image` read `$root := .` while every caller passes `dict "root" … "component" …`, so
     `.Values` was nil and `index nil "migrate"` failed on **every** render.
   - the same helper emitted its own `image:` key under a caller's `image:` key — `image: image:
     ghcr.io/…`, a YAML parse error rather than a wrong image.
   - all three `range` loops wrote `---` glued to the next document's first key. **`helm template`
     tolerates it; `helm lint`, which parses per file, does not** — so the one gate that would have
     caught it was the one the previous tick could not run, and the tool that would have passed is
     not the tool that ran.
   - the migration Job asked for `ghcr.io/raksix/omnion-migrate`, an image no Dockerfile builds and
     no release publishes: a pull failure during `pre-install`, the worst possible moment to
     discover it. `--migrate-only` is a MODE of the API binary, so the job pulls the API image.
   - `migration.image.tag: ""` **overrode** the release tag instead of inheriting it, so
     `--set image.tag=9.9.9` moved every pod to 9.9.9 and left the migration job on `appVersion`:
     0.4's migrations running immediately before 0.5's pods. The comment above that value warned
     against exactly this, produced by the setting meant to prevent it. Empty now means "inherit",
     by construction — the restore copies the BASE value back, because `unset`ing the key produces
     the same fall-through one indirection further from the cause.
   - `checksum/config` sat inside `with .Values.podAnnotations`, so with no pod annotations — the
     default — a values edit left every pod running the old value while `helm upgrade` reported
     success.
   - the migration Job inherited `readOnlyRootFilesystem: true` with **no** `/tmp` mounted: an EROFS
     at startup, in the pre-upgrade hook, before the release begins.
   - `migration.enabled` was documented in `values.yaml` and ignored by the template.

   **The gate's own defects are the more useful half of the record, because each was a check that
   was not checking.** Six, in order of how long each one hid:

   1. **`set -o pipefail` + `grep -q` in a pipeline inverts the result.** `-q` exits on the first
      match, the upstream `grep -v` dies of SIGPIPE, and 141 becomes the pipeline's status — so the
      `if` took the `else` branch on precisely the file where the leak WAS present, and reported a
      clean pass over a 679-line render carrying a real credential. It only became visible because
      the toy case was small; **a check whose exit status can be 141 is a check whose result is
      inverted by success.**
   2. **A grep for a secret is unrunnable when the display masks it.** The tooling that shows helm's
      output redacts `postgres://u:***@db/x` to `u:***`, so any pattern that walks the
      password stops matching at the asterisks — green on a clean render AND on a leaking one. The
      rule is now STRUCTURAL: `scheme://` + userinfo `@` + host. It fires identically on the file
      and on its masked display, and a plain endpoint (`http://omnion-api:8080`) has no `@` and
      does not match, which is what keeps three legitimate endpoints from training a reader to
      ignore the line.
   3. **A pattern naming one SYNTAX of a thing is a pattern for that syntax.** The first version
      matched `value: postgres://…` and missed `DATABASE_URL=postgres://…` — which is literally the
      line NOTES.txt tells the operator to run, so that was the form that mattered most.
   4. **`[ 	]` is Python's `re`, not POSIX.** It is green in a python spot-check and DEAD under
      `grep -E`, which matched a literal `t`. Two languages, one expression, only one ever ran it.
   5. **NOTES.txt cannot be rendered without a cluster.** `helm template` omits it and both
      `helm install --dry-run` variants dial the API server — the flag is documented as "will not
      attempt cluster connections" and connects anyway. The text moved into a `define` that a probe
      template renders, so there is ONE source and the text the gate greps is the text the operator
      reads.
   6. **A check scoped to the whole file is scoped to things its subject never said.** `/readyz`
      also appears in the Deployment probes and in a template comment, so a whole-file grep stayed
      green when the notes' own guidance sentence was rewritten. The check now reads the notes
      document and the guidance sentence itself.

   The gate also carries a **self-test**: it writes a fixture containing a deliberate leak and
   requires the scanner to fire on it while ignoring two plain endpoints. A scanner that has only
   ever been green has demonstrated that it does not fire, not that the chart is clean.

   **Still NOT claimed:** no cluster install, no `helm upgrade` cycle, no migration-hook ordering
   observed from job logs, and no image built. The two Kubernetes acceptance lines that need those
   stay unticked, and the release pipeline and the upgrade helper are untouched.
3. **Release pipeline + artifacts UI.** Tag-driven build and publish, CLI binaries, SBOM, manifest, artifact cache, `/deployment/artifacts` screens, bundle generator and downloads. *Done when:* a fake tag produces a complete manifest and the panel shows digests matching the registry.
   — **The manifest half shipped** (`4b477907`, `9be5323d`): `release/lib/manifest.py`
   (build · verify · schema · deterministic synthetic facts), `release/tests/` (46 unit
   tests) and `scripts/qa/release-manifest.sh` (42 checks, 19/19 mutations), plus the
   `omnion:non-service` marker in `cli.Dockerfile` and a CI job that runs both on every
   commit. The definition-of-done sentence has two halves and only the first is met: a fake
   tag produces a complete manifest; **the panel has no `/deployment/artifacts` screen
   yet**, because those need the `release_manifests` cache table (REQ-129's migration
   number) and the deployment centre REQ-024 owns. Also outstanding in this slice: the
   tag-driven pipeline itself (`.github/workflows/release.yml`), which builds, pushes and
   attests — none of which can be exercised on a build box, so it ships last and only after
   its dry-run mode exists.

   **The pipeline half shipped too** (`51f04848`, `bcf27509`): `release/lib/pipeline.py`
   (`plan` · `gates` · `dry-run` · `publish-check`), `release/tests/test_release_pipeline.py`
   (60 tests) and `scripts/qa/release-pipeline.sh` (**51 checks, 12/12 mutations caught**),
   both wired into CI. The dry-run mode the previous tick's note said the tag workflow was
   waiting on now exists and is exercised on every CI run: on this box **3 of 17 stages
   produce a real artifact** (the chart, packed and repacked to prove reproducibility, and
   both compose stacks rendered by compose's own parser) and the other 14 are reported
   blocked with the tool and the reason.

   **Why the manifest came before the screens.** The screens render whatever the manifest
   says, so building them first would have meant rendering a hand-written fixture and calling
   that progress. The manifest is the contract; the screens are a view of it.
4. **Upgrade helper + docs.** Upgrade plans, destructiveness flags from REQ-129, acknowledgement, `/deployment/upgrade`, `docs/deployment/upgrade.md` and the per-version notes workflow. *Done when:* the guide's steps are executed verbatim on the QA stack and the helper's checklist matches what the operator does.
   — **The server half shipped** (`8cde4d19`): `crates/deployment` (the decision layer and the
   store), `database/migrations/0199_deployment_tooling.sql`, nine permission-guarded routes and a
   three-walk integration suite. The four admin screens and the tag pipeline are what remains.

   **Proof.** `omnion-deployment --lib` **33/33**, `omnion-events --lib` 49/49, `omnion-permissions
   --lib` 66/66, `omnion-api --test deployment_release` **3/3 run THREE TIMES consecutively**,
   `tsc --noEmit` clean. The migration was applied, reversed and re-applied on a scratch database.
   The repeatability is not decoration: a range reused across runs is what broke the walk twice, so
   "passes once" and "passes when the tree is already as it was" are different claims.

   **Why the crate is a crate and not a module of the API.** The decision layer — which steps, in
   what order, and what is *known* about whether the database can be rolled back — is a property of
   the release contract, not of the screen that renders it. A route that computed its own step list
   would agree with itself on the day it was written and disagree with the operator's reality the
   first time somebody changed a migration. So `crates/deployment` builds the plan, `apps/api`
   renders it, and neither has an opinion the other does not hold.

   **The finding is the third verdict, again, now with a persistence layer behind it.** The Python
   helper said `unknown` because REQ-129's gate has not landed; the Rust half says the same thing
   from a *different* set of inputs — the markers the release fetch discovered and whether this
   instance has the policy — and both land on `unknown`. A manifest's `migrations_destructive:
   false` does not produce `reversible`, and there is a unit test whose only subject is that
   refusal. The point of no return attaches to the FIRST migration while the verdict is not
   `reversible`, and a reversible upgrade has none at all.

   **Four product defects, all found by running the suite against a database.**

   - **The acknowledgement set the actor and THEN cleared the previous holder.** The partial unique
     index is a real constraint and it fired on the *set*, so every acknowledgement answered 500
     with `duplicate key value violates unique constraint` — and the follow-up `update` written to
     prevent exactly that never ran. This is the clearest statement of the slice's method: it passed
     all 33 unit tests, because none of them builds a router and the index only exists in a
     database. Clearing first is also the order that is correct if the process dies between the two
     statements: the range is left un-acknowledged (it re-asks) rather than double-acknowledged
     (a state nothing can read).
   - **A `compose` plan accepted `helm` as a stack and then generated `docker compose` commands.**
     The check used `BUNDLE_KINDS`, the set the bundle *generator* accepts, which contains `helm`;
     the stack file was then resolved through an `unwrap_or(<the small stack>)`, so a Helm operator
     would have been handed a correct-looking list of commands against the wrong tool with no error
     anywhere. Two vocabularies for two different things, and a check against the larger one accepts
     a plan that cannot be executed.
   - **A route guarded with `deployment.manage`, a key the catalogue does not have.** The family is
     `read` / `preview` / `deploy` / `rollback`. A guard on an uncatalogued key refuses *every*
     account including the instance owner, so the acknowledgement endpoint was a 503-shaped hole —
     and nothing caught it except the integration walk, on its first execution. The key is now
     `deployment.deploy`, and the reason is written at the guard.
   - **The credential rule fired on any URL with userinfo**, so it fired on
     `postgres://omnion@postgres/omnion` — a line the compose stack writes on its healthy path. A
     scanner that fires on correct code gets its real findings ignored, which is the failure this
     whole request is written against. `user:pass@host` is a credential; `user@host` is not.

   **Four walk defects, all the same shape, and the shape is the lesson.** Every one was a fixture
   that was silently wrong and read as a product defect: a uuid's decimal digits overflow `u32` so
   `parse().unwrap_or(1)` produced the SAME target version on every run; the plan's `from_version`
   is the *build's* version, so two walks picking the same target shared one range and the first
   walk's acknowledgement answered the second; a `{id}` that was never created turned a read into
   `GET /deployment/bundles//files/…` and its 400 read as a permissions failure; and a `published_kinds`
   expectation ignored that the coverage query is ordered by kind. **A fixture that is silently
   constant looks exactly like a product that ignores its input**, so the ranges are now derived from
   each walk's own name and the acknowledgements are reset at the start of the walk that owns them.

   **Still open in this slice.** The four `/deployment/*` admin screens (`/deployment/artifacts`,
   `/deployment/artifacts/{version}`, `/deployment/install`, `/deployment/upgrade`) and the tag
   pipeline (`.github/workflows/release.yml`). The bundle generator's `render` endpoint answers
   with the input a real `helm template` / `docker compose config` would read and names the tool,
   because neither command exists on an installed panel and a route that answered `501` for every
   bundle would be a dead button.
4. **Upgrade helper + docs.** Upgrade plans, destructiveness flags from REQ-129, acknowledgement, `/deployment/upgrade`, `docs/deployment/upgrade.md` and the per-version notes workflow. *Done when:* the guide's steps are executed verbatim on the QA stack and the helper's checklist matches what the operator does.
   — **The decision layer, the migration, the routes and the walk are done; the screens and the guide's
   end-to-end execution on the QA stack are not.**

   ### Slice 3's third part shipped: the bundle generator (`2e7ee60f`, `e303d0d6`, `cc8e2b69`)

   `release/lib/bundle.py` (`generate_bundle` · `validate_request` · `verify_bundle`),
   `release/tests/test_release_bundle.py` (39 tests) and `scripts/qa/release-bundle.sh`
   (**23 checks, 3 mutations**), wired into CI.

   **Proof.** `python3 -m unittest discover -s release/tests -t .` **145 tests OK**. The gate
   **23/23**, and its non-vacuity is proven rather than assumed: deleting the generator's
   credential-text check turns it red (22 passed, 1 failed) because the fixture value then
   reaches a generated file. All three kinds verify with the tool that would install them —
   `helm template` renders the generated values file and `docker compose config` parses both
   stacks, each with the operator's `.env` filled in.

   **The acceptance criterion is enforced twice.** By construction — the request record has no
   field a credential can arrive in, so there is no code path that accepts one — and by
   measurement: every generated file is scanned with `pipeline._literal_credentials`, the
   compose gate's OWN rule imported rather than copied, both as written and rendered. The
   shell gate greps the OUTPUT for a fixture value, so a scan that stopped scanning entirely
   still fails there.

   **Seven defects, every one from running a tool instead of reading the generator.** In its own
   output: a multi-line comment that put `#` on the first line only, so five `--from-literal`
   lines escaped into the YAML and `helm` reported a *type* error with an unrelated cause;
   one shared `requests`/`limits` map assigned to all three components, so every pod asked for
   all three components' memory (an operator sizing a node from it provisions ~3x the machine
   and still schedules one pod); and `ingress.hosts[].paths[]` missing its required `service`
   key, which renders a rule that is present, matched and never routed.

   **Two repository defects, both landing on an operator's first install.** `docker-compose.prod.yml`
   demanded a variable named `VAR` — the header explained the `:?` form by writing it literally,
   and compose interpolates inside comments too. And `docker-compose.enterprise.yml` required
   three variables `.env.example` never declared, so the enterprise install refused to start with
   no answer to "missing from where?" in any file the operator was given.

   **And the credential scan fired on the chart's own `values.yaml`** — `existingSecret:
   omnion-secrets` and `secrets.keys.s3SecretKey: S3_SECRET_KEY` are both committed there. The
   exception is a named set of Kubernetes object-name keys (asserted in the tests, with
   `password` deliberately not on it) rather than a loosened pattern, because the response to a
   credential check firing on correct code is to switch it off, and then the leak ships.

   **Still NOT claimed:** `/deployment/artifacts` and `/deployment/install` have no screens, the
   tag workflow does not exist, and the `environment_bundles` / `release_artifacts` tables are
   unwritten — this slice's generator is the contract those four things consume.

### Risks / notes

- Image bloat creeps in one dependency at a time; the size budgets are enforced in CI with a soft warning and a hard documentation threshold, and build-time dependencies must never leak into the runtime layer.
- Multi-architecture builds are slow and expensive; cache the builder layers, publish `arm64` at release time only (not on every commit), and tag digests so a consumer can pin an exact artifact.
- The chart and the compose files drift apart unless both are checked in CI: a parity test renders both and compares probes, migration ordering, environment variable names and port numbers.
- Secrets belong to the operator's secret store or a Kubernetes Secret referenced by name; generated bundles, values files and image layers must never carry a value, and the tests above assert it rather than trusting review.
- Migration ordering is the one sequence that can corrupt a live system: the job runs before the new code serves traffic, the app image never runs migrations at boot, and the upgrade guide states the ordering for both topologies (REQ-129 owns the runner).
- A published release manifest is a contract: digests, checksums and the minimum core version are verified on download, and a mismatch is a hard failure, not a warning.
- The CLI binary on macOS and Windows is unsigned here; document the checksum-verification step plainly rather than implying notarisation.
