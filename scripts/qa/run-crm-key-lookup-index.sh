#!/usr/bin/env bash
# CRM intake — the public key lookup is actually indexed. (REQ-117, slice 21)
#
#   bash scripts/qa/run-crm-key-lookup-index.sh
#
# ## Why this gate is its own file
#
# `store::find_source_by_key`'s doc comment says:
#
#   > The key is hashed and matched against the stored digest, so this is one indexed equality.
#
# There was no index. `pg_indexes` on a fully migrated database listed five indexes on
# `crm_intake_sources` and none covered `endpoint_key_hash`, so the platform's only deliberately
# unauthenticated business endpoint ran a sequential scan of every tenant's rows on every
# anonymous request — 13.6 ms at 100k sources, growing with the installation rather than with
# the attacker's effort.
#
# ## What makes this gate different from a "the index exists" check
#
# A presence check is what the previous slices shipped and it is exactly the kind of check that
# goes green on a database that has not been migrated. This gate **asks the planner**, on a
# deliberately hostile fixture, and the assertion is the *scan node type* — because:
#
# * presence alone is not usage: PostgreSQL keeps an index it never chose, and a table small
#   enough to fit in a page or two will seq-scan with a perfect index present and be right to;
# * the test has to be big enough that a missing index *must* show, which means the fixture
#   builds a row count rather than trusting whatever the database happens to hold.
#
# So the gate loads rows, `ANALYZEs` (a stale statistic is how a "fixed" index still measures
# as a scan), and reads `explain (analyze, buffers)` off the **real production statement** —
# copied from `store.rs`, columns included, so a change to the query is a change to this gate
# rather than a silent divergence between what is asserted and what runs.
#
# The negative control is the same statement against the same fixture **with the index dropped**
# in a transaction that is rolled back, so the gate proves it can see the defect it exists to
# catch. A gate that has never gone red on its own defect is a gate whose green means nothing.
#
# ## Its own database, never the pass's
#
# These gates open with `DROP DATABASE … WITH (FORCE)`, which terminates the browser pass's API
# connections; the failure then lands twenty routes from the cause — the lesson that cost
# `run-crm-request-id.sh` three ticks of false CRM walkthroughs.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_keyidx}"

if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database ($DB)." >&2
  echo "        Give the gate its own: the other crm gates use omnion_qa_w8_*." >&2
  exit 1
fi

# Lifted byte-level out of a sibling gate rather than retyped: a hand-written URL produces
# "N failed" that is the script's configuration, not a regression, and a tool masks credentials
# in rendered output — the mask is what gets copied.
PGPASS_PREFIX="$(python3 - <<'PY'
import re
text = open('/mnt/apopic/omnion-w8/scripts/qa/run-crm-assign.sh', encoding='utf-8').read()
m = re.search(r'DATABASE_URL="(postgres://[^@"]+@127\.0\.0\.1:5433)/', text)
print(m.group(1) if m else '')
PY
)"
if [ -z "${PGPASS_PREFIX}" ]; then
  echo "  FAIL: could not read the QA database prefix out of run-crm-assign.sh." >&2
  exit 1
fi
export DATABASE_URL="${PGPASS_PREFIX}/${DB}"

psql_q() { docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -A -t -v ON_ERROR_STOP=1 -c "$1"; }

echo "[crm-key-lookup-index] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

PASS=0
FAIL=0
ok()   { echo "  ok: $1"; PASS=$((PASS + 1)); }
bad()  { echo "  FAIL: $1" >&2; FAIL=$((FAIL + 1)); }

# --1-- The migration is applied and the relation the comment names exists at all.
#
# Asked of the catalog rather than of a file on disk, because the failure this gate exists for
# is precisely "the file is right and the database never got it".
INDEXDEF="$(psql_q "select indexdef from pg_indexes
                    where indexname = 'crm_intake_sources_key_lookup_idx'")"
if [ -z "${INDEXDEF}" ]; then
  bad "crm_intake_sources_key_lookup_idx does not exist after a full migration sweep"
else
  ok "the index exists in the migrated schema"
fi

# --2-- Its shape is the one the comment in the migration argues for: partial on the nulls, and
# deliberately NOT unique. Both are load-bearing, and both are the kind of thing a later tidy-up
# "fixes" back into a plain unique index while every test still passes.
case "${INDEXDEF}" in
  *"endpoint_key_hash"*)
    case "${INDEXDEF}" in
      *"UNIQUE"*)
        bad "the index is UNIQUE — a digest reachable on two rows is a state a restored dump leaves behind, and a unique index makes the lookup refuse it for the wrong reason"
        ;;
      *"WHERE"*)
        ok "partial, as argued: a form source stores no digest"
        ;;
      *)
        bad "the index is not partial — every keyless source pays for an index entry that can never match"
        ;;
    esac
    ;;
  *)
    bad "the index does not cover endpoint_key_hash: ${INDEXDEF}"
    ;;
esac

# --3-- The fixture. Enough rows that a missing index cannot possibly win on cost.
#
# One organization, so the scan under test is *within* a tenant and the measurement is not
# diluted by cross-tenant noise; 20k rows is past the point where PostgreSQL prefers a scan,
# and `analyze` is not optional — an unanalyzed table has no statistics and the planner is
# choosing blind, which is how a fixed index still measures as a scan.
psql_q "insert into organizations (id, name, slug, created_at, updated_at)
        values ('11111111-1111-1111-1111-111111111111', 'key idx org', 'key-idx-org', now(), now())
        on conflict (id) do nothing" >/dev/null
psql_q "insert into crm_intake_sources (organization_id, name, kind, endpoint_key_hash, endpoint_key_hint)
        select '11111111-1111-1111-1111-111111111111', 'src-' || g, 'endpoint',
               md5(g::text), 'abcd'
        from generate_series(1, 20000) g" >/dev/null
psql_q "analyze crm_intake_sources" >/dev/null

# --4-- THE ASSERTION: the planner uses the index for the production statement.
#
# The statement is `store::find_source_by_key`'s, columns and predicates copied from `store.rs`.
# Both halves are needed: the digest equality is what the index serves, and `kind` is the
# predicate slice 20 added, so this also proves that adding it did not quietly push the lookup
# back into a scan.
PLAN="$(psql_q "explain (analyze, buffers)
                select id, organization_id, site_id, name, kind, form_key, endpoint_key_hash,
                       endpoint_key_hint, mapping, required_targets, consent_required,
                       consent_text, dedupe_policy, pipeline_id, stage_id, auto_tags,
                       autoresponder, active, rate_limit_per_hour, last_received_at, last_error,
                       broken_mappings, created_by, created_at, updated_at
                from crm_intake_sources
                where endpoint_key_hash = '9e107d9d372bb6826bd81d3542a419d6'
                  and active and kind = 'endpoint'")"

if echo "${PLAN}" | grep -q "Index Scan using crm_intake_sources_key_lookup_idx"; then
  ok "the public lookup is served by the index"
else
  bad "the public lookup is NOT using the index — it is scanning"
  echo "${PLAN}" | sed 's/^/        /' >&2
fi

# The complementary half: a *hit* must use it too. A fixture whose digest matches nothing can
# be answered from the index's metadata alone, so a gate that only ever asserts on a miss is
# asserting on the cheap case.
HITPLAN="$(psql_q "explain (analyze)
                   select id from crm_intake_sources
                   where endpoint_key_hash = md5('1'::text) and active and kind = 'endpoint'")"
if echo "${HITPLAN}" | grep -q "Index Scan using crm_intake_sources_key_lookup_idx"; then
  ok "a matching digest is also served by the index"
else
  bad "a matching digest does not use the index"
  echo "${HITPLAN}" | sed 's/^/        /' >&2
fi

# --5-- THE NEGATIVE CONTROL: the same statement on the same fixture, index dropped.
#
# Rolled back, so the gate leaves the schema as it found it, and run through a transaction
# because `drop index concurrently` cannot run inside one — a plain `drop index` is correct
# here precisely because the rollback undoes it.
NEGATIVE="$(psql_q "begin;
                    drop index crm_intake_sources_key_lookup_idx;
                    explain (analyze)
                    select id, organization_id, site_id, name, kind, form_key, endpoint_key_hash,
                           endpoint_key_hint, mapping, required_targets, consent_required,
                           consent_text, dedupe_policy, pipeline_id, stage_id, auto_tags,
                           autoresponder, active, rate_limit_per_hour, last_received_at, last_error,
                           broken_mappings, created_by, created_at, updated_at
                    from crm_intake_sources
                    where endpoint_key_hash = '9e107d9d372bb6826bd81d3542a419d6'
                      and active and kind = 'endpoint';
                    rollback" 2>&1 || true)"

if echo "${NEGATIVE}" | grep -q "Seq Scan on crm_intake_sources"; then
  ok "PROVEN TO FAIL: with the index dropped the same statement seq-scans, so the gate can see its defect"
else
  bad "PROVEN TO FAIL FAILED: dropping the index did not produce a seq scan — this fixture is too small"
  echo "${NEGATIVE}" | sed 's/^/        /' >&2
fi

# --6-- And the rollback really happened: a gate that measures a defect it then leaves behind
# is a gate that makes the *next* run meaningless.
STILL_THERE="$(psql_q "select count(*) from pg_indexes where indexname = 'crm_intake_sources_key_lookup_idx'")"
if [ "${STILL_THERE}" = "1" ]; then
  ok "the negative control rolled back — the index is still there"
else
  bad "the negative control left the schema changed (index count: ${STILL_THERE})"
fi

echo "[crm-key-lookup-index] ${PASS} passed, ${FAIL} failed"
[ "${FAIL}" -eq 0 ]
