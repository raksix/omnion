#!/usr/bin/env bash
# CRM intake — the terminal status the sweep's first read still walks. (REQ-117, slice 40)
#
#   bash scripts/qa/run-crm-terminal-status.sh
#
# ## Why this gate exists
#
# Slice 39 asked the planner about the SLA sweep's three reads and found its **first** read
# naming no status at all. The fix added the predicate, and the gate asserted the plan *and* the
# answer — which is what made it trustworthy. This tick asks the next question: the predicate it
# added is a hand-written list, and a hand-written list is only correct for as long as nobody
# adds a status.
#
# `not_closed_statuses_sql` is `status not in ('spam', 'rejected', 'duplicate')`. `converted` is
# **not** in that list — and `converted` is terminal. The crate's own vocabulary says so:
# "`converted`, `duplicate`, `spam` and `rejected` are terminal: the SLA clock reads only the
# rows that are still waiting". So the sweep's first read walks every tenant holding a
# converted, unanswered, overdue lead, and both consumer reads return nothing for it.
#
# **Why `converted` leads are unanswered at all.** This is the part that makes it permanent
# rather than merely wrong. `convert_lead` writes `status`, `contact_id`, `deal_id` and
# `converted_at`. `mark_quote_accepted` writes `status`, `converted_at`, `quote_id`. **Neither
# writes `first_response_at`** — and no other path does: `record_response` has exactly one
# caller, the operator's respond route. A lead converted through the panel's own Convert button
# is therefore unanswered for ever, so the sweep names its tenant on every tick until the lead
# happens to be answered by hand.
#
# The two predicates disagree about a status they both think about, and the module has a test
# that says they partition the vocabulary: `the_two_sql_predicates_partition_the_statuses_the_
# platform_knows` compares `is_open` against itself and passes — because both halves are asked
# the same question. Nothing asserts that `not_closed_statuses_sql` excludes everything
# `is_open` excludes, which is the assertion that would have caught this.
#
# ## What this gate asserts
#
# 1. The sweep's first read does NOT name a tenant holding only converted/unanswered/overdue
#    leads — the answer, not the plan.
# 2. It STILL names the tenant that has live work: a narrowing that fixed this by dropping
#    `converted` from the comparison entirely would answer 0 and lose a live clock.
# 3. `count_breached` agrees with the sweep, because the two are the same question asked twice
#    and they are the panel's two surfaces for it.
# 4. The negated predicate's closed list is exactly the complement of `is_open` — as SETS.
# 5. NEGATIVE CONTROL: adding `'converted'` back to the closed list reproduces the defect, so
#    the gate can see it.
#
# ## Its own database, never the pass's
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_terminal}"

if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database (${DB})." >&2
  exit 1
fi

PGPASSWORD="$(python3 - <<'PY'
import re
text = open('/mnt/apopic/omnion-w8/scripts/qa/run-crm-assign.sh', encoding='utf-8').read()
m = re.search(r'DATABASE_URL="(postgres://[^@"]+)@', text)
print(m.group(1).split('://', 1)[1].split(':', 1)[1] if m else '')
PY
)"
if [ -z "${PGPASSWORD}" ]; then
  echo "  FAIL: could not read the QA database password out of run-crm-assign.sh." >&2
  exit 1
fi

# Both predicates come from the crate ITSELF, by running it — not from a regex over its source.
#
# Slice 40's fix made `not_closed_statuses_sql` DERIVED from `STATUSES`, which is what stopped the
# hand-written list from leaking `converted`; that same change killed this gate's reader, which
# recovered both predicates by regexing the `format!` body out of `vocabulary.rs`. It now fails
# with "could not read … out of vocabulary.rs", which reads like a missing constant rather than a
# broken checker. **A checker that reads the code's TEXT stops working when the code stops being
# text-shaped, and it fails in the vocabulary of the thing it is checking.** So the gate asks the
# crate for its answer (`examples/predicates.rs`), and refuses to fall back to guessing — a
# hand-recovered list is a second copy of the rule, which is the whole thing this gate exists to
# delete.
export CARGO_TARGET_DIR="${QA_CARGO_TARGET_DIR:-/dev/shm/w8-target}"
export CARGO_INCREMENTAL=0
PREDICATES="$(cargo run -q -p omnion-module-crm-intake --example predicates 2>/dev/null)" || true
NOT_CLOSED="$(printf '%s\n' "${PREDICATES}" | sed -n 's/^not_closed=//p')"
OPEN="$(printf '%s\n' "${PREDICATES}" | sed -n 's/^open=//p')"

if [ -z "${NOT_CLOSED}" ] || [ -z "${OPEN}" ]; then
  echo "  FAIL: a status predicate could not be read out of vocabulary.rs." >&2
  exit 1
fi

px() { docker exec -e "PGPASSWORD=${PGPASSWORD}" "$CONTAINER" psql -U omnion -d "${2:-$DB}" -q -A -t -v ON_ERROR_STOP=1 -c "$1"; }
pxi() { docker exec -i -e "PGPASSWORD=${PGPASSWORD}" "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1; }

px "DROP DATABASE IF EXISTS ${DB} WITH (FORCE)" postgres >/dev/null
px "CREATE DATABASE ${DB} OWNER omnion" postgres >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  pxi <"$f" >/dev/null 2>&1
done
px "$(cat scripts/qa/fixture-terminal-status.sql)" >/dev/null

PASS=0
FAIL=0
ok()  { echo "  ok: $1"; PASS=$((PASS + 1)); }
bad() { echo "  FAIL: $1" >&2; FAIL=$((FAIL + 1)); }

ORG_BUSY='44444444-4444-4444-4444-000000000010'

# The production statement, verbatim from `organizations_with_leads`.
STATEMENT="select distinct organization_id from crm_leads \
           where first_response_due_at is not null and first_response_at is null \
           and ${NOT_CLOSED} \
           order by organization_id limit 50"

# --1-- THE ANSWER. Nine quiet tenants hold only converted, unanswered, overdue leads. Both
# consumer reads (`due_breaches`, `due_reminders`) filter on `status in ('new', …)`, so walking
# them costs two round trips a minute and produces two empty result sets.
ANSWER="$(px "select string_agg(organization_id::text, ',') from (${STATEMENT}) t")"
COUNT="$(px "select count(*) from (${STATEMENT}) t")"
echo "        sweep names: ${ANSWER}" >&2

if [ "${COUNT}" = "1" ]; then
  ok "the sweep no longer walks the nine tenants whose only leads are converted"
else
  bad "the sweep names ${COUNT} tenants, expected 1 — 'converted' is terminal but not_closed_statuses_sql still admits it, so converted/unanswered/overdue leads are walked every minute for nothing"
fi

# --2-- THE ANSWER THAT MATTERS MOST: the fix must not cost a tenant with a live clock. A
# narrowing that dropped the predicate, or narrowed the wrong end, would answer 0 here and every
# plan-shaped assertion would still be green.
if echo "${ANSWER}" | grep -q "${ORG_BUSY}"; then
  ok "the tenant with four live breached leads is still named"
else
  bad "the sweep no longer names the tenant with live work — the narrowing dropped a live clock"
  echo "        returned: ${ANSWER}" >&2
fi

# --3-- `count_breached` is the SAME question asked by the panel, and the store spells it with
# `not_closed_statuses_sql` — the same predicate, so asserting it here asserts the code's
# spelling rather than a paraphrase. Two surfaces for one number that disagree is the failure
# this gate exists to prevent. (Asserting it with `${OPEN}` instead would pass while the store
# says 13: a gate that tests a predicate the code does not use is a gate that tests itself.)
BREACHED="$(px "select count(*) from crm_leads where organization_id = '${ORG_BUSY}' \
              and first_response_at is null and first_response_due_at is not null \
              and first_response_due_at < now() and ${NOT_CLOSED}")"
if [ "${BREACHED}" = "4" ]; then
  ok "count_breached still counts the busy tenant's four live breaches"
else
  bad "count_breached returned ${BREACHED}, expected 4 — the inbox badge and the sweep must agree about what is overdue"
fi

BREACHED_ALL="$(px "select count(*) from crm_leads \
                   where first_response_at is null and first_response_due_at is not null \
                   and first_response_due_at < now() and ${NOT_CLOSED}")"
if [ "${BREACHED_ALL}" = "4" ]; then
  ok "count_breached counts 4 across every tenant, not 13 (9 converted rows included)"
else
  bad "count_breached returned ${BREACHED_ALL} across all tenants, expected 4 — the inbox badge counts converted leads as breached work"
fi

# --3b-- THE PLAN MUST SURVIVE THE NARROWING. This is the tick-38 trap one level on: a correct
# predicate that the planner cannot prove is a seq scan, and a gate that only asserts the ANSWER
# would be green on a read that got slower. `crm_leads_sla_idx` is partial on the THREE-status
# form, so a FOUR-status predicate *implies* it — but an implication is something to measure, not
# something to assume, and the whole point of the last two ticks was that a rule written in the
# schema does not have to agree with the query.
PLAN="$(px "explain (analyze) ${STATEMENT}")"
echo "${PLAN}" | grep -E 'Index (Scan|Only Scan)|Seq Scan|Bitmap|Execution Time|Rows Removed' | sed 's/^/        /' >&2
if echo "${PLAN}" | grep -q "crm_leads_sla_idx"; then
  ok "the narrowed read is STILL served by crm_leads_sla_idx — the correction cost no plan"
else
  bad "the narrowed read lost crm_leads_sla_idx — the index predicate names three statuses and this one names four, and a seq scan is the cost of a correct-but-unprovable predicate"
  echo "${PLAN}" | sed 's/^/        /' >&2
fi

# --4-- The predicate itself, as the rule it claims to be: every status `is_open` rejects must
# be in the closed list. Compared as SETS, because the two forms are ordered differently in their
# own sources and a text comparison would pass on a reordering.
# The `set -e` trap: a bare `python3 …` that exits 1 would take the whole gate down with it,
# and the gate's own count line would never print — a red gate that looks like a crash rather
# than a finding. The status is captured and turned into a counter here for that reason.
PARTITION="$(python3 - "${NOT_CLOSED}" <<'PY'
import re, sys
closed = set(re.findall(r"'([^']+)'", sys.argv[1]))
# `is_open` and STATUSES are read out of their own definitions: the crate cannot print them as
# SQL (they are Rust predicates), and retyping them here would make this a second copy that
# agrees with itself — the shape of the test it is standing in for.
text = open('/mnt/apopic/omnion-w8/modules/crm-intake/src/vocabulary.rs', encoding='utf-8').read()
mo = re.search(r'pub fn is_open\(status: &str\) -> bool \{\s*\n\s*matches!\(status, ([^)]+)\)', text)
open_set = set(re.findall(r'"([^"]+)"', mo.group(1)))
ms = re.search(r'pub const STATUSES: \[&str; \d+\] = \[([^]]+)\]', text)
all_statuses = set(re.findall(r'"([^"]+)"', ms.group(1)))
missing = (all_statuses - open_set) - closed
extra = closed - (all_statuses - open_set)
if missing or extra:
    print(f"terminal but not excluded: {sorted(missing)}; closed but still open: {sorted(extra)}")
    sys.exit(1)
print("the closed list is exactly the complement of is_open")
PY
)" && PARTITION_OK=1 || PARTITION_OK=0
if [ "${PARTITION_OK}" -eq 1 ]; then
  ok "the closed list is exactly the complement of is_open (as sets)"
else
  bad "not_closed_statuses_sql and is_open do not partition the vocabulary — ${PARTITION}"
fi

# --5-- NEGATIVE CONTROL. The pre-fix list is not retyped here: it is read out of
# `crm_leads_sla_idx`'s partial predicate in migration `0055`, which is the hand-written
# three-status form this slice replaces. A migration is frozen — it can never be "fixed" to
# agree with a later change — so it is a control that stays a control, rather than a literal that
# has to be edited every time the vocabulary moves. It must reproduce the defect: ten tenants.
CONTROL="$(python3 - <<'PY'
import re
text = open('/mnt/apopic/omnion-w8/database/migrations/0055_crm_lead_intake.sql', encoding='utf-8').read()
m = re.search(r'where first_response_at is null and status not in \(([^)]+)\)', text)
if not m:
    raise SystemExit('could not read the SLA index predicate out of 0055')
print('status not in (' + m.group(1) + ')')
PY
)"
# The control's job is to reproduce the DEFECT, not to differ from the code as text. Today the
# index predicate and `not_closed_statuses_sql` are literally the same three-status list — that
# agreement is the defect, and asserting they differ would make this control red for the wrong
# reason before the fix and would have to be edited the moment it became true. What it asserts is
# the behaviour: this predicate names ten tenants, which is what the sweep must not do.
C="$(px "select count(*) from (select distinct organization_id from crm_leads \
          where first_response_due_at is not null and first_response_at is null \
          and ${CONTROL} order by organization_id limit 50) t")"
if [ "${C}" = "10" ]; then
  ok "PROVEN TO FAIL: the index's own three-status predicate names ${C} tenants, so the gate sees the defect"
else
  bad "PROVEN TO FAIL FAILED: the pre-fix predicate named ${C} tenants, expected 10 — this fixture cannot show the defect"
fi

echo "[crm-terminal-status] ${PASS} passed, ${FAIL} failed"
[ "${FAIL}" -eq 0 ]