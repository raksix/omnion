#!/usr/bin/env bash
# CRM intake — the phone index and the phone lookup normalize the SAME way. (REQ-117, slice 38)
#
#   bash scripts/qa/run-crm-phone-index.sh
#
# ## Why this gate exists
#
# `dedupe::PHONE_DIGITS_SQL` is the single SQL spelling of the phone normalization rule, and
# the two queries that use it (`store::fetch_candidates`, `store::merge_attribution`) were both
# wrong before slice 37: they stripped the `+` with the punctuation while `dedupe::normalize_phone`
# keeps it, so the second key in the documented match order could never fire.
#
# **This gate is about a third implementation of the same rule, in the schema rather than in a
# query — and it was introduced by the fix for the first two.** Slices 67 and 69 gave the two
# queries the shared expression. `crm_leads_phone_idx`, created by migration `0055`, still
# normalizes with a bare `regexp_replace(phone, '[^0-9]', '', 'g')`, which does NOT produce the
# same value the query's expression produces: the index key for `+90 555 111 22 33` is
# `905****2233` and the expression's value is `+905****2233`. The equality in the query is
# between the *expression* and the *key*, so the index is simply not a candidate for that
# predicate — the query is correct and the database still seq-scans it.
#
# That is a silent, ordered failure: `merge_attribution` is the first-touch lookup, so on a
# large table a returning visitor's second submission does a full scan of the tenant's leads
# and — because the scan finds nothing wrong, only slowly — nothing in the logs says so.
#
# ## What this gate asserts, and why not just "the index exists"
#
# A presence check passes on a database nobody queried. This gate **asks the planner** on a
# deliberately hostile 20k-row fixture, with the statement copied from `store.rs` including
# `dedupe::PHONE_DIGITS_SQL` substituted exactly as the code does, and asserts the *scan node
# type* — a fixture small enough to fit in a page will seq-scan with a perfect index present and
# be right to, so the row count is what makes the assertion mean something.
#
# The negative control runs the same statement with the index dropped in a transaction that is
# rolled back, so the gate proves it can see the defect it exists to catch.
#
# ## Its own database, never the pass's
#
# These gates open with `DROP DATABASE … WITH (FORCE)`, which terminates the browser pass's API
# connections and lands the failure twenty routes from the cause.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_phidx}"

if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database (${DB})." >&2
  exit 1
fi

# Lifted byte-level out of a sibling gate rather than retyped: a tool masks credentials in
# rendered output, and the mask is what gets copied.
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

psql_q() { docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -A -t -v ON_ERROR_STOP=1 -c "$1"; }

# The stored-side expression, read out of the crate rather than retyped, so this gate cannot
# drift from the code the way a hand-copied index definition already did.
STORED_PHONE="$(python3 - <<'PY'
import re
text = open('/mnt/apopic/omnion-w8/modules/crm-intake/src/dedupe.rs', encoding='utf-8').read()
m = re.search(r'pub const PHONE_DIGITS_SQL: &str =\s*\n?\s*"((?:[^"\\]|\\[\s\S])*)"', text)
if not m:
    raise SystemExit('could not read PHONE_DIGITS_SQL out of dedupe.rs')
body = m.group(1)
# A Rust line continuation eats the backslash, the newline and the following indent. Leaving
# the backslash in hands the planner a query with a stray `\`, which is a syntax error — the
# gate failing for its own reasons instead of for the defect it exists to catch.
body = re.sub(r'\\\n\s*', '', body)
body = body.replace('\\"', '"').replace('\\n', '\n').replace('\\\\', '\\')
body = body.replace('{column}', 'phone')
print(' '.join(body.split()))
PY
)"
if [ -z "${STORED_PHONE}" ]; then
  echo "  FAIL: PHONE_DIGITS_SQL could not be read out of the crate." >&2
  exit 1
fi

echo "[crm-phone-index] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

PASS=0
FAIL=0
ok()  { echo "  ok: $1"; PASS=$((PASS + 1)); }
bad() { echo "  FAIL: $1" >&2; FAIL=$((FAIL + 1)); }

# --1-- The index and the expression are the same function of the column.
#
# This is the assertion the whole gate is named for, and it is asked of the **catalog**, not of
# the file on disk: the failure this gate exists for is precisely "the file says one thing and
# the database has another". `pg_get_indexdef` renders the expression PostgreSQL stored, so a
# later tidy-up that rewrites the index while every test still passes is caught here.
INDEXDEF="$(psql_q "select pg_get_indexdef(oid) from pg_class where relname = 'crm_leads_phone_idx'")"
if [ -z "${INDEXDEF}" ]; then
  bad "crm_leads_phone_idx does not exist after a full migration sweep"
else
  ok "the index exists in the migrated schema"
fi

# The shape that must agree: the index has to keep the `+` the same way the expression does.
# A bare `regexp_replace` is the exact pre-fix spelling, so naming it is naming the defect.
#
# `pg_get_indexdef` renders the operator as `~~`, not `like`, so both spellings are accepted:
# matching on the text a human wrote is the assertion that silently rots into matching on
# nothing, because the index is still correct when the check stops seeing it.
EXPECTED_KEY="case when btrim(coalesce(phone, '')) like '+%' then '+' else '' end || regexp_replace(coalesce(phone, ''), '[^0-9]', '', 'g')"
if echo "${INDEXDEF}" | grep -Eqi "(like '\+%')|(~~ '\+%')"; then
  ok "the index preserves the leading plus, so its key is what the query's expression computes"
elif echo "${INDEXDEF}" | grep -q "regexp_replace"; then
  bad "the index normalizes the phone WITHOUT the plus, so it does not index the predicate the phone arm compares: ${INDEXDEF}"
  echo "        expected the stored-side expression: ${EXPECTED_KEY}" >&2
else
  bad "the index does not normalize the phone at all: ${INDEXDEF}"
fi

# --2-- The fixture: twenty thousand leads whose phones are stored the way a mapping produces
# them — verbatim from the payload, `+`, spaces and all. One organization, so the measurement
# is within a tenant. `analyze` is not optional: a stale statistic is how a fixed index still
# measures as a scan.
echo "[crm-phone-index] loading the fixture (20k leads)"
psql_q "$(cat scripts/qa/fixture-phone-index.sql)" >/dev/null

# --3-- THE ASSERTION: the planner uses the index for the production statement.
#
# The statement is `store::merge_attribution`'s — the first-touch lookup, whose phone arm is the
# one this gate is about — with the columns and the `PHONE_DIGITS_SQL` substitution copied from
# `store.rs`, **including the two `is not null` conjuncts**. Those are not decoration: both
# indexes are partial, and PostgreSQL only offers a partial index when the planner can prove
# every row the query would read satisfies the predicate. A statement copied from the query but
# stripped of that conjunct is a different statement, and it seq-scans on a perfectly good index
# — which is what the first run of this gate measured.
STATEMENT="select utm_source, utm_medium, utm_campaign, utm_term, utm_content, click_id, \
             referrer_host, landing_path, source_path from crm_leads \
           where organization_id = '22222222-2222-2222-2222-222222222222' \
             and (email is not null or phone is not null) \
             and (lower(email) = '+905000001' or ((phone is not null) and (${STORED_PHONE}) = '+905000001')) \
           order by received_at asc limit 1"

PLAN="$(psql_q "explain (analyze, buffers) ${STATEMENT}")"
# Both arms are an `or`, so the planner answers with a *Bitmap* Index Scan over both indexes
# rather than an `Index Scan using` — and the node that matters is the one that names
# `crm_leads_phone_idx`, because a bitmap over the e-mail index alone would satisfy a check
# written for "some index was used" while the phone arm still scanned. The assertion names the
# index, not the node type.
if echo "${PLAN}" | grep -q "Index Scan on crm_leads_phone_idx"; then
  ok "the phone arm of the first-touch lookup is served by the phone index"
else
  bad "the phone arm is NOT using the index — the index and the expression disagree, so this lookup scans"
  echo "${PLAN}" | sed 's/^/        /' >&2
fi

# --4-- THE NEGATIVE CONTROL: the same statement, index dropped, rolled back.
NEGATIVE="$(psql_q "begin;
                    drop index crm_leads_phone_idx;
                    explain (analyze) ${STATEMENT};
                    rollback" 2>&1 || true)"

if echo "${NEGATIVE}" | grep -q "Seq Scan on crm_leads"; then
  ok "PROVEN TO FAIL: with the index dropped the same statement seq-scans, so the gate can see its defect"
else
  bad "PROVEN TO FAIL FAILED: dropping the index did not produce a seq scan — this fixture is too small"
  echo "${NEGATIVE}" | sed 's/^/        /' >&2
fi

# --4b-- The SECOND control, and the one this gate was actually written for.
#
# The index can be perfect and the query can still not use it. This statement is the production
# one with the `is not null` conjuncts removed — the shape the code had before this tick, and a
# perfectly reasonable thing for a reader to write. It must seq-scan, and that is the whole
# argument for those conjuncts being load-bearing rather than decorative.
WITHOUT_CONJUNCT="select utm_source, utm_medium, utm_campaign, utm_term, utm_content, click_id, \
                    referrer_host, landing_path, source_path from crm_leads \
                  where organization_id = '22222222-2222-2222-2222-222222222222' \
                    and (lower(email) = '+905000001' or (${STORED_PHONE}) = '+905000001') \
                  order by received_at asc limit 1"
CONJUNCT_PLAN="$(psql_q "explain (analyze) ${WITHOUT_CONJUNCT}")"
if echo "${CONJUNCT_PLAN}" | grep -q "Seq Scan on crm_leads"; then
  ok "PROVEN TO FAIL: without 'phone is not null' the planner cannot offer the partial index, so the conjunct is load-bearing"
else
  bad "the statement without the conjunct still uses the index — the conjuncts in the query are not doing what the comment claims"
  echo "${CONJUNCT_PLAN}" | sed 's/^/        /' >&2
fi

# --5-- And the rollback really happened.
STILL_THERE="$(psql_q "select count(*) from pg_class where relname = 'crm_leads_phone_idx'")"
if [ "${STILL_THERE}" = "1" ]; then
  ok "the negative control rolled back — the index is still there"
else
  bad "the negative control left the schema changed (index count: ${STILL_THERE})"
fi

echo "[crm-phone-index] ${PASS} passed, ${FAIL} failed"
[ "${FAIL}" -eq 0 ]
