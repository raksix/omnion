# Upgrading an Omnion install

This guide covers both topologies end to end. It is written against what the code actually
does, and every rule in it has a gate behind it:

| Rule | Enforced by |
|---|---|
| Migrations run before new code serves traffic | `service_completed_successfully` in the prod stack; a `pre-install`/`pre-upgrade` hook Job in the chart |
| An application rollback is always available | the previous image tag; no schema involved |
| A database rollback exists only for a verified down script | REQ-129's `up → down → up` gate — **not built yet** |
| The helper will not call a migration reversible before that gate exists | `release/lib/upgrade.py`, `destructiveness()` |
| Every command below parses before you run it | `scripts/qa/release-upgrade.sh` runs `bash -n` over each one |

The panel's upgrade helper (`/deployment/upgrade`) renders this as a checklist for your
installed version. The generated plan is the source for the step order; this document is the
source for the judgement calls, which no plan can make for you.

---

## 1. What an upgrade is, and what it is not

An upgrade is **three actions in a fixed order**: a backup, a migration, a deploy. The order
is not a convention — each of the two topologies enforces it mechanically:

- **compose** — the `migrate` service is a one-shot job, and `api` carries
  `depends_on: migrate: condition: service_completed_successfully`. The API cannot start
  until the migration exits zero. `service_started` would let the API boot in parallel with
  the migration, which is the exact failure the job exists to prevent.
- **Kubernetes** — the migration is a `pre-install`/`pre-upgrade` **hook** Job with a hook
  weight, so `helm upgrade` holds the release until it finishes. A hook is not a Deployment:
  it is not managed, not autoscaled, and not rolled back with the workloads.

Both run the same binary in the same mode: `--migrate-only` is a mode of the API image, not
a second image. That is deliberate — the migration code and the code that reads the schema
are one build, and two images is how a "migration added in 0.4" ends up running 0.5's DDL
under 0.5's pods.

**What an upgrade is not:** a swap of the image tag. The tag is the last step, and by the
time you change it the schema has already moved.

---

## 2. The rollback split

This is the distinction the whole guide is built around, and getting it wrong is the
expensive mistake:

- **Application rollback** — the previous image tag. Always available, takes seconds, and
  touches no data. If the new version boots and then misbehaves, this is your move.
- **Database rollback** — running the down script. Available **only** for releases whose
  migrations shipped one that CI verified by applying it and reversing it. Otherwise the
  path is **restoring a backup**, which is slower and loses everything written since.

The two are not the same, and the difference matters at exactly one moment: the point of no
return, below.

### The point of no return

Where the two stop agreeing, and it moves depending on the migrations:

| Migrations in the range | Point of no return | Why |
|---|---|---|
| All reversible | the **last** migration step | before it, the old image works against either schema; after it, the old image needs the down script first |
| Any destructive, **or unknown** | the **first** migration step | from there the schema moves one way only and the backup is the way back |

The helper marks the step. A marker you have to infer from a numbered list is a marker
nobody notices, which is why the plan carries `point_of_no_return` as an index into the
rendered list rather than as prose.

### "Unknown" is the current answer, and it is not "none"

**REQ-129 has not landed.** Until its `up → down → up` gate exists, a migration file with no
`-- omnion:no-down` marker has *not been proven reversible* — it has merely not been
declared irreversible. The helper renders that as `unknown`, keeps `database_rollback`
as `unknown`, and refuses to complete its checklist without an acknowledgement.

A release manifest's `migrations_destructive: false` does **not** override this. That flag
says the publisher's tree had no marker; it is the publisher's silence, not a verification.

---

## 3. Compose — the small-company stack

Assumes you installed from a generated bundle (`/deployment/install`); the files are
`docker-compose.prod.yml` and `.env`.

```bash
# 1. BACKUP — before anything else. The database rollback path below is a restore from
#    this file, so it has to exist before the first migration.
docker compose -f docker-compose.prod.yml exec -T postgres \
  pg_dump -Fc -U "$OMNION_DB_USER" -d "${OMNION_DB_NAME:-omnion}" > omnion-0.5.0.dump

# 2. MIGRATE — the one-shot job. api is gated on this exiting zero.
docker compose -f docker-compose.prod.yml run --rm migrate

# 3. DEPLOY — the application, last.
OMNION_IMAGE_TAG=0.5.0 docker compose -f docker-compose.prod.yml up -d --no-deps api admin web

# 4. VERIFY — readiness, not liveness. A draining or dependency-broken instance answers
#    /healthz while /readyz still refuses.
curl -fsS https://<your domain>/readyz
```

**Application rollback** (always available):

```bash
OMNION_IMAGE_TAG=0.4.0 docker compose -f docker-compose.prod.yml up -d --no-deps api admin web
```

**Database rollback**: if the range was reversible, the migration runner's down path runs
the verified down script. If it was destructive or unknown, restore `omnion-0.5.0.dump`
and accept the data written since.

### Enterprise topology

Same steps, `docker-compose.enterprise.yml`, and the datastores are **external** — there is
no `postgres` service to `exec` into:

```bash
# 1. BACKUP — through your own database tooling. The stack deliberately has no database
#    container, so this command is NOT `docker compose exec`.
pg_dump -Fc -U "$OMNION_DB_USER" -d "${OMNION_DB_NAME:-omnion}" -h "$OMNION_DB_HOST" > omnion-0.5.0.dump

# 2. MIGRATE / 3. DEPLOY — identical, against the external endpoints in .env.
docker compose -f docker-compose.enterprise.yml run --rm migrate
OMNION_IMAGE_TAG=0.5.0 docker compose -f docker-compose.enterprise.yml up -d --no-deps api admin web
```

A silent `127.0.0.1` default in the enterprise stack would turn an enterprise install into a
single-host one with no warning, so every endpoint there is a required reference with no
fallback. If the stack starts, the endpoints resolved.

---

## 4. Kubernetes

```bash
# 1. BACKUP — a snapshot of the external database. A database rollback is a restore from
#    this, so it precedes the first migration.
#    (cloud provider snapshot, or a volume snapshot)

# 2. MIGRATE + DEPLOY — one command. The migration is a pre-upgrade hook, so this cannot
#    complete until the migration Job exits zero, and --wait holds the rollout too.
helm upgrade omnion infra/helm/omnion --version 0.5.0 --set image.tag=0.5.0 --wait

# 3. VERIFY
kubectl rollout status deploy/omnion-api
kubectl get pods -l app.kubernetes.io/instance=omnion
```

**`helm rollback` is not the whole story.** It reverts the manifests, and the manifests
include the migration hook — so `helm rollback` attempts the down script for the release
you are rolling back *from*. With a destructive or unknown migration in the range that is a
Job that must not run by accident: **restore the snapshot from step 1 instead.**

With a verified down script, `helm rollback <release> --to-revision <previous>` is the fast
path.

---

## 5. Post-upgrade verification

Do all of these. Each catches a failure the previous one does not.

1. `curl -fsS https://<your domain>/readyz` → 200. Readiness, not `/healthz`.
2. `docker compose -f docker-compose.prod.yml ps` (or `kubectl get pods`) → every service
   **running/ready**, none restarting. A service in a restart loop passes step 1 often enough
   to be missed.
3. The migration is recorded: the one-shot job's exit code was 0 and the schema version the
   API reports matches the target's migration set. An API that serves traffic against an
   older schema reports 503 on readiness, so a green `/readyz` with a stale schema means the
   API is on a different database than the job migrated.
4. Sign in and open a screen that reads migrated data. A schema that applied but a backfill
   that did not is a green health check and an empty table.
5. Errors: `/observability/alerts` (REQ-126) and the API log explorer. An upgrade that
   breaks a query shows as errors, not as a failed boot.

---

## 6. Per-version notes

Each release carries `notes_md` and `upgrade_notes_url` in its manifest, and the panel
renders them next to the plan. Add notes under this heading for the version you are
shipping; the panel reads the manifest, not this file, so a note that exists only here is
invisible to an operator using the helper.

### 0.5.0

- No per-version notes recorded yet. The first release with a published manifest writes its
  section here at tag time.

---

## 7. What is not proven yet

Stated plainly, because a guide that overstates its own coverage is worse than a short one:

- **No migration has been verified reversible on this repository.** The `up → down → up`
  gate is REQ-129's slice 1 and it has not been built. Every plan today reports `unknown`.
- **The plan's steps have not been executed on a running stack.** The gate parses every
  command with `bash -n` and checks every stack file exists and every chart path resolves,
  but nobody has run the sequence against a live compose or Kubernetes install.
- **No image has been built or pushed** from this branch, so the image reference in a plan
  is derived from the manifest's registry and repository, not from a registry that answered.
