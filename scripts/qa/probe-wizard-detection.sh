#!/usr/bin/env bash
# Prove the wizard-detection defect is closed, at the level it lives.
#
# The defect: `runWizard` decided "the installation already exists" from the browser's
# address bar. That address is a *lagging* witness — an anonymous `/` is redirected to
# `/login`, and `/login` only continues to `/setup` after a `GET /onboarding` round trip
# inside a useEffect. Sampling it after a fixed wait reads "not in setup" on a fresh
# database, and the pass then walks a database with no owner, no organization and no site.
#
# This probe answers the same question the fixed harness asks — of the API, which is where
# the state actually lives — against a database that is fresh and one that is not, and it
# fails loudly if the two answers ever agree.
set -euo pipefail

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
API_PORT="${QA_API_PORT:-18089}"
DB="omnion_qa_w8_wizard"
BASE="http://127.0.0.1:${API_PORT}"

pass=0
fail=0
check() { # name, expected, actual
  if [ "$2" = "$3" ]; then
    echo "  ok   $1"
    pass=$((pass + 1))
  else
    echo "  FAIL $1: expected [$2], got [$3]"
    fail=$((fail + 1))
  fi
}

# A fresh database: `needs_setup` must be true, because `users::has_any` counts nobody.
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null 2>&1

OMNION_DATABASE_URL="postgres://omnion:omnion@127.0.0.1:5433/${DB}" \
OMNION_REDIS_URL="redis://127.0.0.1:6380" \
OMNION_CSRF_SECRET="probe-only-not-production" \
OMNION_PORT="$API_PORT" \
OMNION_ENV=development \
  ./target/debug/omnion-api >/tmp/w8_wizprobe.log 2>&1 &
api_pid=$!
trap 'kill $api_pid 2>/dev/null || true' EXIT

for _ in $(seq 1 60); do
  code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "${BASE}/healthz" || true)"
  [ "$code" = "200" ] && break
  sleep 1
done
check "the probe API answers" "200" "${code:-none}"

# 1. The fresh database says it needs setup. This is the sentence the old harness inferred
#    from a lagging redirect and got wrong.
fresh="$(curl -s "${BASE}/api/v1/onboarding" | python3 -c 'import json,sys; print(json.load(sys.stdin)["needs_setup"])')"
check "a fresh database answers needs_setup=true" "True" "${fresh}"

# 2. The response must actually carry the fields the harness reads. A fix that queried
#    `needs_setup` alone would leave `summary.organization_name` unchecked, and the tenant
#    assertion in walkthrough.cjs depends on it — so a rename on the API side would turn the
#    pass's own guard into a `TypeError` instead of a clear refusal.
fields="$(curl -s "${BASE}/api/v1/onboarding" | python3 -c '
import json, sys
body = json.load(sys.stdin)
print("ok" if all(k in body for k in ("needs_setup", "completed", "steps", "summary")) else "missing")')"
check "the status route carries every field the harness reads" "ok" "${fields}"

# 3. After an account exists the answer flips, so the fix is not a constant `true`.
docker exec "$CONTAINER" psql -U omnion -d "$DB" -q \
  -c "insert into users (id, email, display_name, password_hash, created_at, updated_at)
      select gen_random_uuid(), 'probe@omnion.test', 'Probe',
             'x', now(), now()
      where not exists (select 1 from users);" >/dev/null
after="$(curl -s "${BASE}/api/v1/onboarding" | python3 -c 'import json,sys; print(json.load(sys.stdin)["needs_setup"])')"
check "an installation with an account answers needs_setup=false" "False" "${after}"

echo "  ---"
echo "  ${pass} passed, ${fail} failed"
[ "$fail" -eq 0 ] || exit 1
