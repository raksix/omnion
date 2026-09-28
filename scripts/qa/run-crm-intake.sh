#!/usr/bin/env bash
# REQ-117's slice-1 gate: the intake migration and its constraints against a real database.
#
# The unit tests prove the mapping, the transforms, the dedupe arithmetic and the spam
# scoring — all of which are pure. None of them can prove that the *migration applies* on top
# of the released set, that the check constraints agree with the crate's closed lists, or that
# the indexes the inbox's sort depends on exist. Those are statements about a live database,
# so this is a disposable stack: its own database, dropped at the end.
#
#   bash scripts/qa/run-crm-intake.sh
#
# It answers six questions the unit tests cannot:
#   1. does 0051 apply cleanly on top of the released set?
#   2. does a lead with neither e-mail nor phone get refused by the constraint?
#   3. does the status/decision/kind/policy vocabulary refuse what the crate refuses?
#   4. is a payload over the ceiling refused rather than truncated?
#   5. does deleting a lead cascade its trail while leaving the rows that pointed at it?
#   6. do the indexes the inbox's sort and slice 2's worker read actually exist?
#
# **Every `psql` call in this file carries `ON_ERROR_STOP=1`**, and every answer is *compared*
# rather than printed. The first version of this gate proved neither: a `returning … \gset`
# failed, every later statement in the heredoc was a syntax error, and the script still printed
# `PASS` — a gate that reports success while testing nothing is worse than no gate, because the
# next reader trusts it. Hence the `run_sql` helper: one definition, so a new call cannot
# forget the flag.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"
# A NOTICE about a trigger that does not exist yet is not a failure; an ERROR is.
export PGOPTIONS="-c client_min_messages=warning"

DB="omnion_qa_crm_intake"
PGHOST=127.0.0.1
PGPORT=5433
export PGPASSWORD=omnion
ORG_ONE=aaaaaaaa-0000-0000-0000-000000000001
ORG_TWO=aaaaaaaa-0000-0000-0000-000000000002

# One way to talk to the database, so no call site can forget ON_ERROR_STOP.
PSQL_ADMIN=(psql -h "$PGHOST" -p "$PGPORT" -U omnion -d postgres -v ON_ERROR_STOP=1 -q)
PSQL=(psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -v ON_ERROR_STOP=1 -q -t -A)

cleanup() { "${PSQL_ADMIN[@]}" -c "drop database if exists $DB" >/dev/null 2>&1 || true; }
trap cleanup EXIT

# Assert that a statement is refused. Used for every value the crate refuses and the database
# must refuse too: accepted by one and refused by the other is a filter that silently returns
# nothing, or a status the panel has no label for.
assert_refused() {
  local label="$1" statement="$2"
  if "${PSQL[@]}" -c "$statement" >/dev/null 2>&1; then
    echo "  FAIL: the database accepted $label, which the crate refuses"
    exit 1
  fi
  echo "  ok: $label refused"
}

# Assert that a scalar read equals what it must be. The value is read from the database again
# rather than parsed out of the transcript: a `grep` on this script's own output would also
# match a line printed while the statement underneath had failed.
assert_scalar() {
  local label="$1" expected="$2" statement="$3" actual
  actual="$("${PSQL[@]}" -c "$statement")"
  if [ "$actual" != "$expected" ]; then
    echo "  FAIL: $label — expected $expected, got $actual"
    exit 1
  fi
  echo "  ok: $label = $actual"
}

echo "[crm-intake] creating a disposable database"
"${PSQL_ADMIN[@]}" -c "drop database if exists $DB" >/dev/null
"${PSQL_ADMIN[@]}" -c "create database $DB"

# Apply file-by-file, in filename order — the same order sqlx applies them, so a migration that
# only works *after* a later one cannot pass here. ON_ERROR_STOP makes the first failure fatal.
echo "[crm-intake] applying the migration set (this is the gate on 0051)"
for f in database/migrations/*.sql; do
  "${PSQL[@]}" -q -f "$f" >/dev/null
done
echo "  applied $(find database/migrations -name '*.sql' | wc -l) migrations, 0051 included"

echo "[crm-intake] the schema landed"
assert_scalar "tables" "3" \
  "select count(*) from information_schema.tables
    where table_name in ('crm_intake_sources','crm_leads','crm_lead_events')"
assert_scalar "indexes" "7" \
  "select count(*) from pg_indexes
    where indexname in ('crm_leads_inbox_idx','crm_leads_sla_idx','crm_leads_dedupe_idx',
                        'crm_leads_email_idx','crm_leads_phone_idx','crm_lead_events_lead_idx',
                        'crm_intake_sources_form_key')"

echo "[crm-intake] seeding two organizations and a source each"
"${PSQL[@]}" >/dev/null <<SQL
insert into organizations (id, name, slug) values
  ('$ORG_ONE', 'Acme', 'acme'),
  ('$ORG_TWO', 'Globex', 'globex');
insert into crm_intake_sources (organization_id, name, kind, consent_required) values
  ('$ORG_ONE', 'Quote form', 'endpoint', false),
  ('$ORG_TWO', 'Quote form', 'endpoint', false);
SQL
# The uniqueness is per organization, so two tenants may each own a source with the same name —
# the case that must pass. The complementary case below must fail.
assert_scalar "two tenants may share a source name" "2" \
  "select count(*) from crm_intake_sources where name = 'Quote form'"

echo "[crm-intake] the constraints the crate relies on"

# A lead with neither e-mail nor phone cannot be contacted, so it is not a lead. The crate
# refuses it in `contactable` before the write; the constraint refuses it if something routes
# around the crate.
assert_refused "a lead with no e-mail and no phone" \
  "insert into crm_leads (organization_id, first_name) values ('$ORG_ONE', 'Nobody')"

# The four closed lists, both directions.
assert_refused "status 'won'" \
  "insert into crm_leads (organization_id, status, email) values ('$ORG_ONE', 'won', 'a@example.test')"
assert_refused "decision 'maybe'" \
  "insert into crm_leads (organization_id, decision, email) values ('$ORG_ONE', 'maybe', 'a@example.test')"
assert_refused "kind 'webhook'" \
  "insert into crm_intake_sources (organization_id, name, kind) values ('$ORG_ONE', 'Hook', 'webhook')"
assert_refused "dedupe policy 'always'" \
  "insert into crm_intake_sources (organization_id, name, dedupe_policy) values ('$ORG_ONE', 'Always', 'always')"

# A payload over the ceiling is *refused*, not truncated: a truncated payload produces a lead
# whose fields do not match what the visitor sent, which is the failure nobody can see.
assert_refused "a payload over 256 KiB" \
  "insert into crm_leads (organization_id, email, payload_bytes) values ('$ORG_ONE', 'a@example.test', 262145)"
assert_refused "a spam score over 100" \
  "insert into crm_leads (organization_id, email, spam_score) values ('$ORG_ONE', 'a@example.test', 101)"
# A rate limit of zero refuses every submission, and the form looks broken with no explanation.
assert_refused "a rate limit of 0" \
  "insert into crm_intake_sources (organization_id, name, rate_limit_per_hour) values ('$ORG_ONE', 'Zero', 0)"
# A second source of the same name inside one organization.
assert_refused "a second 'Quote form' in one organization" \
  "insert into crm_intake_sources (organization_id, name, kind, consent_required)
   values ('$ORG_ONE', 'Quote form', 'endpoint', false)"

echo "[crm-intake] a lead, its trail, and what a delete takes with it"
LEAD_ID="$("${PSQL[@]}" -c \
  "insert into crm_leads (organization_id, status, email, first_name, last_name, payload, payload_bytes)
   values ('$ORG_ONE', 'new', 'ada@acme.test', 'Ada', 'Lovelace', '{}'::jsonb, 2) returning id")"
if [ -z "$LEAD_ID" ]; then
  echo "  FAIL: the lead insert returned no id, so nothing below was tested"
  exit 1
fi
echo "  lead=$LEAD_ID"

# The organization id is passed in as a variable rather than interpolated into the heredoc:
# a `<<'SQL'` heredoc does not expand `$ORG_ONE`, and the failure reads as a uuid syntax error
# rather than as "you forgot the expansion" — which is exactly how the first version of this
# gate managed to print PASS over four syntax errors.
"${PSQL[@]}" -v org_one="$ORG_ONE" -v lead_id="$LEAD_ID" >/dev/null <<'SQL'
insert into crm_lead_events (lead_id, kind, detail)
values (:'lead_id', 'received', '{"status":"new"}'::jsonb);
-- A duplicate that points at the lead above. A duplicate queue that loses rows when a merge is
-- reversed is a queue nobody can undo a mistake in.
insert into crm_leads (organization_id, status, email, duplicate_of)
values (:'org_one', 'duplicate', 'ada2@acme.test', :'lead_id');
SQL

assert_scalar "the trail was written" "1" \
  "select count(*) from crm_lead_events where lead_id = '$LEAD_ID'"

"${PSQL[@]}" -c "delete from crm_leads where id = '$LEAD_ID'" >/dev/null

assert_scalar "the trail cascaded with its lead" "0" \
  "select count(*) from crm_lead_events"
assert_scalar "the duplicate survived with a null pointer" "1" \
  "select count(*) from crm_leads where email = 'ada2@acme.test' and duplicate_of is null"

echo "[crm-intake] the indexes the inbox's sort and slice 2's worker read"
# The inbox sorts by (first_response_due_at is null, first_response_due_at, received_at), so a
# missing `crm_leads_sla_idx` is a sort over the whole table on every page load of the screen
# slice 2 exists to make urgent.
for index in crm_leads_inbox_idx crm_leads_sla_idx crm_leads_email_idx crm_lead_events_lead_idx; do
  assert_scalar "$index exists" "1" "select count(*) from pg_indexes where indexname = '$index'"
done

echo "[crm-intake] PASS"
