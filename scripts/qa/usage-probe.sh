#!/usr/bin/env bash
# The usage probe's SQL, run against the live database with no `graph` column at all.
# The unit test asserts the *shape* of the expression; this asserts it *parses and runs* on an
# install that has not merged the builder's migration, which is the only way to catch a
# `to_jsonb` that does not do what the comment says.
set -uo pipefail
DB="${QA_DB:-omnion_qa_w10}"
EXPR='coalesce(
        case when jsonb_typeof(to_jsonb(w) -> '"'"'graph'"'"' -> '"'"'nodes'"'"') = '"'"'array'"'"'
             then to_jsonb(w) -> '"'"'graph'"'"' -> '"'"'nodes'"'"' end,
        case when jsonb_typeof(to_jsonb(w) -> '"'"'steps'"'"') = '"'"'array'"'"'
             then to_jsonb(w) -> '"'"'steps'"'"' end,
        '"'"'[]'"'"'::jsonb)'

run() { PGPASSWORD=omnion psql -h 127.0.0.1 -p 5433 -U omnion -d "$DB" -tAc "$1" 2>&1; }

echo "== the usage probe, on a schema with no \`graph\` column =="

has_graph=$(run "select count(*) from information_schema.columns where table_name='workflows' and column_name='graph'")
echo "  graph column present: $has_graph"

# 1. The expression parses and runs at all.
out=$(run "select count(*) from workflows w, lateral jsonb_array_elements($EXPR) as n where n->'params'->>'credential_key' = 'anything'")
echo "  probe on an empty table: $out"
if echo "$out" | grep -q "does not exist"; then
  echo "  FAIL the probe does not parse without \`graph\`"
  exit 1
fi
echo "  ok   the probe parses without \`graph\`"

# 2. A row with only `steps` is read, and its credential reference is found.
PGPASSWORD=omnion psql -h 127.0.0.1 -p 5433 -U omnion -d "$DB" -q -c "
  insert into workflows (organization_id, name, trigger_kind, steps)
  select (select id from organizations limit 1), 'probe fixture', 'manual',
    '[{\"name\":\"Call API\",\"kind\":\"task\",\"action\":\"http_request\",
       \"params\":{\"credential_key\":\"probe-key\"}}]'::jsonb
  where not exists (select 1 from workflows where name = 'probe fixture');" >/dev/null 2>&1
out=$(run "select n->'params'->>'credential_key' from workflows w, lateral jsonb_array_elements($EXPR) as n where n->'params'->>'credential_key' = 'probe-key'")
if [ "$out" = "probe-key" ]; then
  echo "  ok   a \`steps\`-only workflow is read and its reference found"
else
  echo "  FAIL expected probe-key, got: $out"
fi

# 3. The count is one, not two: the same row must not be read twice by the coalesce.
count=$(run "select count(*) from workflows w, lateral jsonb_array_elements($EXPR) as n where n->'params'->>'credential_key' = 'probe-key'")
if [ "$count" = "1" ]; then
  echo "  ok   the row is counted once"
else
  echo "  FAIL expected 1, got: $count"
fi

# 4. `referenced_keys` uses the same expression and does not double-count either.
distinct=$(run "select count(distinct n->'params'->>'credential_key') from workflows w, lateral jsonb_array_elements($EXPR) as n where n->'params' ? 'credential_key'")
if [ "$distinct" = "1" ]; then
  echo "  ok   referenced_keys is distinct"
else
  echo "  FAIL expected 1 distinct key, got: $distinct"
fi

# 5. Clean up the fixture so a later pass does not inherit it.
PGPASSWORD=omnion psql -h 127.0.0.1 -p 5433 -U omnion -d "$DB" -q -c \
  "delete from workflows where name = 'probe fixture'" >/dev/null 2>&1
echo "  ok   fixture removed"
