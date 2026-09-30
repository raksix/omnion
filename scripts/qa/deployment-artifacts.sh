#!/usr/bin/env bash
# Omnion deployment artifacts — the properties a production stack must have (REQ-128, slice 1).
#
# This gate exists because a compose file that PARSES is not a compose file that is SAFE, and
# because the four failures below are all invisible to review:
#
#   1. A missing `:?` on a credential silently becomes an empty string. The stack boots, the
#      API connects, and the database is open. Only a refusal at parse time is loud.
#   2. `read_only: true` missing on one service means one service has a writable root, and
#      nobody notices because the other five have it.
#   3. The `migrate` job that is supposed to gate the API is either not a dependency, or is a
#      dependency on the wrong condition (`service_started` lets the API boot in parallel with
#      the migration, which is the exact failure the job exists to prevent).
#   4. A `RUN`/`ENV`/`ARG` line that carries a credential-looking value. This is the
#      acceptance line "no image layer or build history contains a secret", and it is the one
#      that a human reviewer reliably waves through.
#
# It uses `docker compose config` — compose's OWN parser and schema — rather than a YAML library
# or a regex, so a check cannot pass because it read the file differently than compose does.
# `docker compose config` also performs the variable substitution, which is what makes check 1
# meaningful: it is the substitution that turns `${VAR:?}` into a refusal.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
COMPOSE_DIR="$ROOT/infra/compose"

pass=0
fail=0
check() { # description, expected, actual
  if [ "$2" = "$3" ]; then
    printf 'ok   %s\n' "$1"
    pass=$((pass + 1))
  else
    printf 'FAIL %s\n       expected: %s\n       actual:   %s\n' "$1" "$2" "$3"
    fail=$((fail + 1))
  fi
}

if ! command -v docker >/dev/null 2>&1; then
  # A single-quoted bash string cannot contain an escaped apostrophe: `\'` ends the string,
  # the quote opens again on the next character, and the parser then reports a missing `done`
  # hundreds of lines later with no mention of this line. The possessive is rewritten instead.
  printf "SKIP docker is not installed; these checks need the compose parser\n"
  exit 0
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# A full set of values, because `config` performs the substitution and refuses on an unset
# required variable — which is itself check 1.
export OMNION_DB_USER=gate OMNION_DB_PASSWORD=gate OMNION_S3_ACCESS_KEY=gate
export OMNION_S3_SECRET_KEY=gate OMNION_PUBLIC_URL=https://gate.test OMNION_CSRF_SECRET=gate
export OMNION_ADMIN_EMAIL=gate@gate.test OMNION_ADMIN_PASSWORD=gate

render() { # <compose file> -> the normalised config on stdout
  docker compose -f "$1" config 2>"$tmp/err"
}

# ---------------------------------------------------------------------------------------------
# 1. The stack parses under compose's own schema.
# ---------------------------------------------------------------------------------------------
prod="$COMPOSE_DIR/docker-compose.prod.yml"
if render "$prod" > "$tmp/prod.yml" 2>"$tmp/err"; then
  printf 'ok   the production stack parses under `docker compose config`\n'; pass=$((pass + 1))
else
  printf 'FAIL the production stack does not parse\n'; sed 's/^/       /' "$tmp/err" | head -5
  fail=$((fail + 1))
  # Every later check reads the rendered file. Parsing failed, so stop rather than report a
  # dozen confusing absences as separate failures.
  printf '\n%d passed, %d failed\n' "$pass" "$fail"
  exit 1
fi

# The services the request names, each present exactly once.
services="$(docker compose -f "$prod" config --services | sort | tr '\n' ' ')"
check "the stack ships api, admin, web, migrate and the three datastores" \
  "admin api migrate minio postgres redis web " "$services"

# ---------------------------------------------------------------------------------------------
# 2. Required credentials refuse to start rather than defaulting to empty.
# ---------------------------------------------------------------------------------------------
# Render with each required variable UNSET. `:?` makes compose exit non-zero with a message
# naming the variable; a plain `${VAR}` would render an empty value and the check below would
# pass, which is the failure this is for.
for var in OMNION_DB_PASSWORD OMNION_S3_SECRET_KEY OMNION_CSRF_SECRET OMNION_ADMIN_PASSWORD; do
  ( unset "$var"; docker compose -f "$prod" config >/dev/null 2>&1 )
  rc=$?
  if [ "$rc" -ne 0 ]; then
    printf 'ok   %s is required: compose refuses to render without it\n' "$var"
    pass=$((pass + 1))
  else
    printf 'FAIL %s is not required: compose rendered the stack without it\n' "$var"
    fail=$((fail + 1))
  fi
done

# And the file must not carry a value for one of them. A comment naming the variable is fine;
# an assignment is not.
leaked="$(grep -nE '^[[:space:]]*OMNION_(DB_PASSWORD|S3_SECRET_KEY|CSRF_SECRET|ADMIN_PASSWORD)=.+' \
  "$COMPOSE_DIR/.env.example" 2>/dev/null | grep -v '=$' | wc -l | tr -d ' ')"
check ".env.example assigns no value to a credential" "0" "$leaked"

# ---------------------------------------------------------------------------------------------
# 3. Migration ordering: the API waits for a SUCCESSFUL migration, not a started one.
# ---------------------------------------------------------------------------------------------
# `service_started` is the trap: the API would boot in parallel with the migration job, which is
# the exact race the job exists to remove. This reads the rendered dependency graph, so it sees
# what compose will actually do rather than what the file looks like it says.
migrate_condition="$(
  python3 - "$tmp/prod.yml" <<'PY'
import sys, yaml
d = yaml.safe_load(open(sys.argv[1]))
dep = (d.get("services", {}).get("api", {}).get("depends_on") or {}).get("migrate") or {}
print(dep.get("condition", "MISSING"))
PY
)"
check "the API waits for the migration to complete successfully" \
  "service_completed_successfully" "$migrate_condition"

# The job must be a one-shot: `restart: unless-stopped` on a migrate service restarts it on
# exit, and a job that restarts forever is a stack that never becomes healthy.
migrate_restart="$(
  python3 - "$tmp/prod.yml" <<'PY'
import sys, yaml
d = yaml.safe_load(open(sys.argv[1]))
print(d.get("services", {}).get("migrate", {}).get("restart", "MISSING"))
PY
)"
check "the migration job does not restart after it finishes" "no" "$migrate_restart"

# And it must actually run the migration, not boot a second API that never exits.
migrate_cmd="$(
  python3 - "$tmp/prod.yml" <<'PY'
import sys, yaml
d = yaml.safe_load(open(sys.argv[1]))
cmd = d.get("services", {}).get("migrate", {}).get("command")
print(" ".join(cmd) if isinstance(cmd, list) else (cmd or "MISSING"))
PY
)"
check "the migration job invokes the one-shot migration mode" \
  "--migrate-only" "$migrate_cmd"

# ---------------------------------------------------------------------------------------------
# 4. Read-only root filesystem, non-root user and the health probe.
# ---------------------------------------------------------------------------------------------
# Every long-running service gets a read-only root; the datastores are excluded because
# PostgreSQL, Redis and MinIO write inside their own data directories, which a read-only root
# would break. Counting them in would mean removing the flag from the app services to make the
# number look right, which is the failure mode a count-only assertion invites.
ro_missing="$(
  python3 - "$tmp/prod.yml" <<'PY'
import sys, yaml
d = yaml.safe_load(open(sys.argv[1]))
need = ["api", "admin", "web", "migrate"]
missing = [s for s in need if d.get("services", {}).get(s, {}).get("read_only") is not True]
print(",".join(missing) or "none")
PY
)"
check "api, admin, web and the migration job all set read_only" "none" "$ro_missing"

# The API healthcheck must use the binary, not a shell tool: the runtime is distroless and has
# neither `curl` nor `wget`, so `curl -f /healthz` fails forever on a healthy container.
probe="$(
  python3 - "$tmp/prod.yml" <<'PY'
import sys, yaml
d = yaml.safe_load(open(sys.argv[1]))
test = d.get("services", {}).get("api", {}).get("healthcheck", {}).get("test") or []
print(" ".join(map(str, test)))
PY
)"
case "$probe" in
  *omnion-api*--healthcheck*) printf 'ok   the API healthcheck calls the binary, which distroless can run\n'; pass=$((pass + 1)) ;;
  *) printf 'FAIL the API healthcheck does not use the binary: %s\n' "$probe"; fail=$((fail + 1)) ;;
esac

# And the binary must actually implement the flag the probe calls. A Dockerfile referencing a
# flag the Rust binary does not parse produces an image that is permanently unhealthy, which
# looks like a broken deploy rather than a missing subcommand.
api_src="$ROOT/apps/api/src/main.rs"
if grep -q '"--healthcheck"' "$api_src" 2>/dev/null; then
  printf 'ok   the API binary implements the flag its healthcheck calls\n'; pass=$((pass + 1))
else
  printf 'FAIL the healthcheck calls --healthcheck but the binary does not handle it\n'; fail=$((fail + 1))
fi
if grep -q '"--migrate-only"' "$api_src" 2>/dev/null; then
  printf 'ok   the API binary implements the migration mode the stack depends on\n'; pass=$((pass + 1))
else
  printf 'FAIL the stack runs --migrate-only but the binary does not handle it\n'; fail=$((fail + 1))
fi

# ---------------------------------------------------------------------------------------------
# 5. No credential in a build argument, a RUN line or an ENV default (the layer-scan criterion).
# ---------------------------------------------------------------------------------------------
# The scan is over the committed Dockerfiles. A match is reported with its line, because a
# gate that says only "a secret was found" cannot be acted on.
for df in "$ROOT"/infra/docker/*.Dockerfile; do
  [ -f "$df" ] || continue
  name="$(basename "$df")"
  # An ARG or ENV whose NAME is credential-shaped and whose VALUE is not empty and not a
  # build-time label is the failure. `${OMNION_CSRF_SECRET}` in an ENV is a reference, which is
  # allowed and is what the enterprise file needs; a literal value is not.
  hits="$(
    grep -nE '^[[:space:]]*(ARG|ENV)[[:space:]]+[A-Za-z0-9_]*(PASSWORD|SECRET|TOKEN|APIKEY|API_KEY|ACCESS_KEY)[A-Za-z0-9_]*=' \
      "$df" 2>/dev/null | grep -vE '=[[:space:]]*("\$\{[^}]*\}")?[[:space:]]*$' | wc -l | tr -d ' '
  )"
  check "$name carries no credential-shaped build argument" "0" "$hits"

  # The runtime user must not be root. A missing USER line means the image runs as whatever the
  # base image defaults to, which for a Debian-slim base is root.
  #
  # The character class includes `:` because distroless's non-root user is written
  # `nonroot:nonroot` (name:group) and a check that accepts only a bare name reports the two
  # safest images in the set as the two that run as root. That is worse than no check: it is a
  # false alarm, and a false alarm is how a gate gets ignored.
  if grep -qE '^[[:space:]]*USER[[:space:]]+[a-z][a-z0-9_:-]*[[:space:]]*$' "$df"; then
    printf 'ok   %s runs as a non-root user\n' "$name"; pass=$((pass + 1))
  else
    printf 'FAIL %s does not set a non-root USER\n' "$name"; fail=$((fail + 1))
  fi

  # Size budget documented where a reader looks for it, not only in the request.
  if grep -q 'io.omnion.size-budget=' "$df"; then
    printf 'ok   %s documents its size budget as an OCI label\n' "$name"; pass=$((pass + 1))
  else
    printf 'FAIL %s does not document a size budget\n' "$name"; fail=$((fail + 1))
  fi
done

# ---------------------------------------------------------------------------------------------
# 6. The Dockerfiles describe a multi-stage build (at least two named stages, one of them a
#    runtime that is not the first stage).
# ---------------------------------------------------------------------------------------------
for df in "$ROOT"/infra/docker/*.Dockerfile; do
  [ -f "$df" ] || continue
  name="$(basename "$df")"
  stages="$(grep -cE '^FROM ' "$df")"
  if [ "$stages" -ge 2 ]; then
    printf 'ok   %s is multi-stage (%s stages)\n' "$name" "$stages"; pass=$((pass + 1))
  else
    printf 'FAIL %s has %s stage(s); the size budget needs a separate build layer\n' "$name" "$stages"
    fail=$((fail + 1))
  fi
done

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
