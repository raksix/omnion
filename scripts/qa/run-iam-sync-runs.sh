#!/usr/bin/env bash
# The sync-run ledger gate (REQ-065, slice 4).
#
#   bash scripts/qa/run-iam-sync-runs.sh
#
# Unit tests prove the Rust. This proves the *schema*, which is the half they cannot see:
#
#   1. does 0119 apply cleanly on top of the released set, in filename order?
#   2. do the closed `kind` / `status` vocabularies actually refuse what the Rust enum refuses?
#      A check constraint wider than the parser is a constraint that never fires.
#   3. does the finish-shape constraint refuse a run that claims to be finished with no date, and
#      a run still going that already has one?  Both are rows a list screen would render happily.
#   4. can a counter go negative?  "-3 users deactivated" has no repair for the operator reading it.
#   5. does a POPULATED run table survive, does the group link upsert instead of duplicating, and
#      does deleting a provider cascade to all three tables?
#   6. does the derived `subject_key` collapse a repeat subject to one retryable unit while a
#      code-only failure stays addressable?
#
# Usage: bash scripts/qa/run-iam-sync-runs.sh
set -uo pipefail
cd "$(dirname "$0")/../.."

DB="${IAM_SYNC_RUNS_DB:-omnion_qa_iam_sync_runs}"
PASS=0
FAIL=0

ok()   { PASS=$((PASS + 1)); echo "  ok   $*"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL $*"; }

psql_db() { PGPASSWORD=omnion psql -h 127.0.0.1 -p 5433 -U omnion -d "$1" -v ON_ERROR_STOP=1 "${@:2}"; }
# Answer `sql` and succeed when the database says no.
refuse() {
  local sql="$1" label="$2" out
  out=$(psql_db "$DB" -tAc "$sql" 2>&1) && { fail "$label (it accepted)"; return 0; }
  case "$out" in
    *"violates check constraint"*|*"duplicate key"*|*"violates foreign key"*) ok "$label — $(printf '%s' "$out" | head -1 | cut -c1-58)" ;;
    *) fail "$label — refused for the wrong reason: $(printf '%s' "$out" | head -1 | cut -c1-58)" ;;
  esac
}
accept() {
  local sql="$1" label="$2" out
  out=$(psql_db "$DB" -tAc "$sql" 2>&1) && ok "$label" || fail "$label (it refused: $(printf '%s' "$out" | head -1 | cut -c1-58))"
}
count_is() {
  local sql="$1" want="$2" label="$3" got
  got=$(psql_db "$DB" -tAc "$sql" 2>/dev/null | tr -d '[:space:]')
  [ "$got" = "$want" ] && ok "$label" || fail "$label (wanted $want, got '$got')"
}

cleanup() { PGPASSWORD=omnion dropdb -h 127.0.0.1 -p 5433 -U omnion --if-exists "$DB" 2>/dev/null; }
trap cleanup EXIT
cleanup

echo "[iam-sync-runs] 1. the migration set in filename order (the gate on 0119)"
psql_db postgres -c "create database \"$DB\"" >/dev/null
COUNT=0
for file in $(find database/migrations -name '*.sql' | sort); do
  if psql_db "$DB" -q -f "$file" >/dev/null 2>&1; then
    COUNT=$((COUNT + 1))
  else
    fail "migration $(basename "$file") did not apply"
    psql_db "$DB" -q -f "$file" 2>&1 | head -3
    exit 1
  fi
done
[ "$COUNT" -gt 0 ] && ok "$COUNT migrations applied, 0119 included" || { fail "no migration applied"; exit 1; }

echo "[iam-sync-runs] 2. the tables and their indexes landed"
count_is "select count(*) from information_schema.tables where table_name in ('directory_sync_runs','directory_sync_errors','provider_group_links')" 3 "three tables exist"
count_is "select count(*) from pg_indexes where indexname in ('directory_sync_runs_provider_started_idx','directory_sync_runs_provider_status_idx','directory_sync_errors_run_idx','directory_sync_errors_subject_idx','provider_group_links_provider_label_idx')" 5 "every specified index exists"

echo "[iam-sync-runs] 3. a populated provider table, then a populated run table"
psql_db "$DB" -q <<'SQL'
insert into organizations (id, name, slug) values
  ('11111111-1111-1111-1111-111111111111', 'Acme', 'acme');
insert into users (id, organization_id, email, display_name, password_hash)
  values ('33333333-3333-3333-3333-333333333333', '11111111-1111-1111-1111-111111111111', 'ops@acme.test', 'Ops', 'x')
  on conflict do nothing;
insert into auth_providers (id, organization_id, slug, name, kind) values
  ('bbbbbbbb-0000-0000-0000-000000000001', '11111111-1111-1111-1111-111111111111', 'okta', 'Okta', 'oidc');
SQL
PROVIDER=bbbbbbbb-0000-0000-0000-000000000001
ACME=11111111-1111-1111-1111-111111111111

psql_db "$DB" -q <<SQL
insert into directory_sync_runs (provider_id, kind, status, started_at, finished_at, users_seen, users_created, users_updated, users_deactivated, groups_seen, error_count, triggered_by) values
  ('$PROVIDER','full','ok',      now() - interval '2 hours', now() - interval '2 hours' + interval '41s', 120, 3, 0, 1, 7, 0, '33333333-3333-3333-3333-333333333333'),
  ('$PROVIDER','delta','partial', now() - interval '1 hour',  now() - interval '1 hour'  + interval '9s',    8, 0, 2, 0, 0, 2, null),
  ('$PROVIDER','manual','failed', now() - interval '5 min',   now() - interval '5 min'  + interval '2s',    1, 0, 0, 0, 0, 1, null),
  ('$PROVIDER','full','running',  now() - interval '10s',    null,                                 40, 1, 1, 0, 2, 0, null);
SQL
count_is "select count(*) from directory_sync_runs" 4 "four runs survive on a populated table"
count_is "select count(*) from directory_sync_runs where status='running' and finished_at is null" 1 "the running run carries no finished_at"
count_is "select count(*) from directory_sync_runs where status<>'running' and finished_at is not null" 3 "the three finished runs all carry one"

echo "[iam-sync-runs] 4. every constraint, asserted as a refusal"
refuse "insert into directory_sync_runs (provider_id, kind, status, finished_at) values ('$PROVIDER','smtp','ok',now())" "an unknown kind"
refuse "insert into directory_sync_runs (provider_id, kind, status, finished_at) values ('$PROVIDER','full','maybe',now())" "an unknown status"
refuse "insert into directory_sync_runs (provider_id, kind, status, finished_at) values ('$PROVIDER','full','ok', null)" "a finished run with no finished_at"
accept "insert into directory_sync_runs (provider_id, kind, status, finished_at) values ('$PROVIDER','full','ok', now())" "a finished run WITH one is ordinary"
refuse "insert into directory_sync_runs (provider_id, kind, status, finished_at) values ('$PROVIDER','full','running', now())" "a running run that already has one"
refuse "update directory_sync_runs set users_deactivated = -3 where id = (select id from directory_sync_runs where kind='full' and status='ok' limit 1)" "a negative count"
refuse "update directory_sync_runs set error_count = -1 where id = (select id from directory_sync_runs where status='running' limit 1)" "a negative error count on the running row"
accept "update directory_sync_runs set users_deactivated = 0 where id = (select id from directory_sync_runs where kind='full' and status='ok' limit 1)" "a zero count is ordinary, not a refusal"
accept "update directory_sync_runs set users_deactivated = 3 where id = (select id from directory_sync_runs where kind='full' and status='ok' limit 1)" "and the count goes back up"
refuse "insert into directory_sync_runs (provider_id, kind, status, finished_at) values ('00000000-0000-0000-0000-000000000000','full','ok',now())" "a run for a provider that does not exist"
refuse "insert into provider_group_links (provider_id, external_id, member_count) values ('$PROVIDER','g-negative',-1)" "a negative member count"
# The duplicate needs a first row to be a duplicate of: a constraint that refuses a repeat is
# only proved against a row that is really there.
psql_db "$DB" -q -c "insert into provider_group_links (provider_id, external_id) values ('$PROVIDER','eng')" >/dev/null
refuse "insert into provider_group_links (provider_id, external_id) values ('$PROVIDER','eng')" "the same external group twice"

echo "[iam-sync-runs] 5. the group link upserts rather than duplicating"
# The 'eng' row already exists from step 4, so the first statement here is itself the upsert
# path. A fresh external id proves the insert path on its own.
accept "insert into provider_group_links (provider_id, external_id, external_label, member_count) values ('$PROVIDER','sales','Sales',4)" "a first sighting of a new group inserts"
count_is "select count(*) from provider_group_links where external_id='sales'" 1 "one row for the new group"
accept "insert into provider_group_links (provider_id, external_id, external_label, member_count, last_seen_at)
       values ('$PROVIDER','eng','Engineering (EU)',14, now())
       on conflict (provider_id, external_id) do update
         set external_label = excluded.external_label,
             member_count    = excluded.member_count,
             last_seen_at    = excluded.last_seen_at" "the second sighting upserts"
count_is "select count(*) from provider_group_links where external_id='eng'" 1 "one row, not two"
count_is "select member_count from provider_group_links where external_id='eng'" 14 "the upsert wrote the new count"

echo "[iam-sync-runs] 6. the failures are per-subject, and the retry key is derived"
RUN=$(psql_db "$DB" -tAc "select id from directory_sync_runs where status='failed' limit 1" | tr -d '[:space:]')
psql_db "$DB" -q <<SQL
insert into directory_sync_errors (run_id, subject, code, message) values
  ('$RUN','ops@acme.test','attribute_invalid','department is required'),
  ('$RUN','ops@acme.test','timeout','the directory did not answer'),
  ('$RUN','','bind_failed','the service account was refused');
SQL
count_is "select count(*) from directory_sync_errors where run_id='$RUN'" 3 "three failures on one run"
count_is "select count(distinct subject_key) from directory_sync_errors where run_id='$RUN'" 2 "a repeat subject and a code-only failure are two retryable keys"
accept "update directory_sync_runs set error_count = (select count(*) from directory_sync_errors where run_id='$RUN') where id='$RUN'" "the run's count can be brought level with its failures"

echo "[iam-sync-runs] 7. deleting a provider takes all three tables with it"
psql_db "$DB" -q -c "delete from auth_providers where id='$PROVIDER'" >/dev/null
count_is "select count(*) from directory_sync_runs" 0 "the runs cascaded"
count_is "select count(*) from directory_sync_errors" 0 "the failures cascaded"
count_is "select count(*) from provider_group_links" 0 "the group links cascaded"
count_is "select count(*) from organizations where id='$ACME'" 1 "the organization itself is untouched"

echo ""
if [ "$FAIL" -eq 0 ]; then echo "PASS $PASS/$PASS"; exit 0; fi
echo "FAIL $FAIL of $((PASS + FAIL))"
exit 1
