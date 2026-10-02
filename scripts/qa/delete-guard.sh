#!/usr/bin/env bash
# The delete guard's refusal, end to end.
#
# `credential-contract.sh` proves the guard *allows* what nothing references. A guard that only
# ever allows is not a guard, so this proves the other direction against a workflow that really
# names the credential: the delete is refused with `credential_in_use`, the refusal names the
# workflow, `details.references` carries the node, and a forced delete then removes the row and
# still reports what it broke.
set -uo pipefail

API="${API:-http://127.0.0.1:18089}"
DB="${QA_DB:-omnion_qa_w10}"
EMAIL="qa-owner@omnion.test"
PASSWORD="OmnionQa-Passw0rd-2026!"
STAMP="$(date +%s)"
KEY="guarded-$STAMP"
NAME="Guarded $STAMP"
JAR=$(mktemp)
pass=0; fail=0

check() {
  # `set -u` makes a missing $2 an error rather than a FAIL, and an error inside a check
  # hides the fact that the check never ran — which is worse than a failure.
  local outcome="${2:-1}"
  if [ "$outcome" = "0" ]; then printf '  ok   %s\n' "$1"; pass=$((pass+1));
  else printf '  FAIL %s\n' "$1"; fail=$((fail+1)); fi
}
field() { grep -o "\"$2\"[[:space:]]*:[[:space:]]*\"[^\"]*\"" "$1" | head -1 | sed 's/.*:[[:space:]]*"//; s/"$//'; }
cleanup() {
  PGPASSWORD=omnion psql -h 127.0.0.1 -p 5433 -U omnion -d "$DB" -q -c \
    "delete from workflows where name = '$NAME'" >/dev/null 2>&1
  rm -f "$JAR" /tmp/guard-*.json
}
trap cleanup EXIT

j() { curl -s -c "$JAR" -b "$JAR" -H 'content-type: application/json' "$@"; }

echo "== REQ-087 slice 2 · the delete guard's refusal =="
login=$(j -o /dev/null -w '%{http_code}' -X POST \
  -d "{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}" "$API/api/v1/auth/login")
check "login ($login)" "$([ "$login" = 200 ] && echo 0 || echo 1)"
if grep -q omnion_session "$JAR" 2>/dev/null; then
  check "the session cookie was stored" 0
else
  check "the session cookie was stored" 1
  echo "     (every request below would be a 401; that is the jar, not the API)"
  exit 1
fi

# A credential…
code=$(j -o /tmp/guard-create.json -w '%{http_code}' -X POST \
  -d "{\"name\":\"$NAME\",\"key\":\"$KEY\",\"type\":\"api_key\"}" "$API/api/v1/credentials")
check "create the credential ($code)" "$([ "$code" = 201 ] && echo 0 || echo 1)"
ID=$(field /tmp/guard-create.json id)

# …and a workflow whose graph names it, written the way the runner's `steps` array holds it.
written=$(PGPASSWORD=omnion psql -h 127.0.0.1 -p 5433 -U omnion -d "$DB" -tAc \
  "insert into workflows (organization_id, name, trigger_kind, steps)
   select (select id from organizations limit 1), '$NAME', 'manual',
     '[{\"name\":\"Call the API\",\"kind\":\"task\",\"action\":\"http_request\",
        \"params\":{\"credential_key\":\"$KEY\"}}]'::jsonb
   returning id" 2>&1)
if echo "$written" | grep -qE '^[0-9a-f-]{36}$'; then
  check "a workflow naming the credential exists" 0
else
  check "a workflow naming the credential exists" 1
  echo "     ($written)"
fi

# The usage view must see it — derived, not stored.
code=$(j -o /tmp/guard-usage.json -w '%{http_code}' "$API/api/v1/credentials/$ID/usage")
INUSE=$(grep -o '"in_use"[[:space:]]*:[[:space:]]*[a-z]*' /tmp/guard-usage.json | head -1 | sed 's/.*:[[:space:]]*//')
WF=$(grep -o '"workflow_count"[[:space:]]*:[[:space:]]*[0-9]*' /tmp/guard-usage.json | head -1 | sed 's/.*:[[:space:]]*//')
check "usage → 200 (got $code)" "$([ "$code" = 200 ] && echo 0 || echo 1)"
check "  …reports in_use:true (got $INUSE)" "$([ "$INUSE" = true ] && echo 0 || echo 1)"
check "  …counts one workflow (got $WF)" "$([ "$WF" = 1 ] && echo 0 || echo 1)"
check "  …names the workflow" "$(grep -q "$NAME" /tmp/guard-usage.json && echo 0 || echo 1)"
check "  …and the node" "$(grep -q 'Call the API' /tmp/guard-usage.json && echo 0 || echo 1)"

# The delete must be refused, with the list attached.
code=$(j -o /tmp/guard-del.json -w '%{http_code}' -X DELETE "$API/api/v1/credentials/$ID")
CODE=$(field /tmp/guard-del.json code)
check "delete of a referenced credential → 409 (got $code)" "$([ "$code" = 409 ] && echo 0 || echo 1)"
check "  …with credential_in_use (got $CODE)" "$([ "$CODE" = credential_in_use ] && echo 0 || echo 1)"
check "  …details carry the workflow" "$(grep -q "workflow_name" /tmp/guard-del.json && echo 0 || echo 1)"
check "  …details carry the node label" "$(grep -q 'Call the API' /tmp/guard-del.json && echo 0 || echo 1)"
check "  …details carry the node type" "$(grep -q 'http_request' /tmp/guard-del.json && echo 0 || echo 1)"

# The row must still be there: a refused delete that deleted anyway is the worst outcome.
code=$(j -o /dev/null -w '%{http_code}' "$API/api/v1/credentials/$ID")
check "the credential survived the refusal (got $code)" "$([ "$code" = 200 ] && echo 0 || echo 1)"

# A forced delete goes through, and still says what it broke.
code=$(j -o /tmp/guard-force.json -w '%{http_code}' -X DELETE "$API/api/v1/credentials/$ID?force=true")
DELETED=$(grep -o '"deleted"[[:space:]]*:[[:space:]]*[a-z]*' /tmp/guard-force.json | head -1 | sed 's/.*:[[:space:]]*//')
BROKE=$(grep -o '"workflow_count"[[:space:]]*:[[:space:]]*[0-9]*' /tmp/guard-force.json | head -1 | sed 's/.*:[[:space:]]*//')
check "forced delete → 200 (got $code)" "$([ "$code" = 200 ] && echo 0 || echo 1)"
check "  …and deleted:true (got $DELETED)" "$([ "$DELETED" = true ] && echo 0 || echo 1)"
check "  …reporting what it broke (got $BROKE)" "$([ "$BROKE" = 1 ] && echo 0 || echo 1)"
check "  …naming the workflow it broke" "$(grep -q "$NAME" /tmp/guard-force.json && echo 0 || echo 1)"

# And the workflow keeps its (now dangling) reference rather than being rewritten behind the
# reader's back: the guard degrades, it does not edit somebody's automation.
# The result goes through a file, not a `$( … )` in the call — a newline between a `check`
# label and its argument splits it into two commands, and the second one runs as a bare word.
ref_file=$(mktemp)
PGPASSWORD=omnion psql -h 127.0.0.1 -p 5433 -U omnion -d "$DB" -tAc \
  "select steps::text from workflows where name = '$NAME'" >"$ref_file" 2>&1
if grep -q "$KEY" "$ref_file"; then degraded=0; else degraded=1; fi
rm -f "$ref_file"
check "the workflow still names the key (the guard degrades, it does not rewrite)" "$degraded"

echo
echo "== $pass passed, $fail failed =="
[ "$fail" = 0 ]
