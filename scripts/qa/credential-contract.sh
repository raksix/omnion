#!/usr/bin/env bash
# REQ-087 slice 2 — prove the credential contract against a live API.
#
# The browser pass is queued behind three other stacks, and the claims this slice makes are
# about the API's *answers*, not about pixels. So this drives the same endpoints directly,
# against the same private w10 stack, and fails loudly on any claim that does not hold:
#
#   1. create → 201, and the response has no secret and no handle
#   2. a secret inside `settings` → credential_secret_write_only, naming the field
#   3. a secret re-sent on PATCH → credential_secret_write_only, and the row is unchanged
#   4. test with no secret → ok:false, health untested, and a reason
#   5. usage on an unreferenced credential → in_use:false, empty
#   6. delete an unreferenced credential → deleted:true (a guard that refuses forever is broken)
#   7. delete again → 404
#   8. an unknown filter → 400 naming the legal set
#
# The secret below is a fixture. It is asserted absent from every response body.
set -uo pipefail

API="${API:-http://127.0.0.1:18089}"
EMAIL="${QA_EMAIL:-qa-owner@omnion.test}"
PASSWORD="${QA_PASSWORD:-OmnionQa-Passw0rd-2026!}"
FIXTURE="qa-fixture-secret-$(date +%s)"
STAMP="$(date +%s)"
JAR="$(mktemp)"
pass=0; fail=0

check() { # name, condition-result
  if [ "$2" = "0" ]; then printf '  ok   %s\n' "$1"; pass=$((pass+1));
  else printf '  FAIL %s\n' "$1"; fail=$((fail+1)); fi
}

login() {
  # The session cookie is HttpOnly and curl only keeps a cookie whose set-cookie arrived on the
  # same invocation that writes the jar — so `-c` has to be on the login call itself, not on a
  # later one. Getting this wrong fails every request afterwards with `organization_required`,
  # which looks exactly like an authorization bug and is not one.
  code=$(curl -s -c "$JAR" -o /tmp/qa-login.json -w '%{http_code}' \
    -H 'content-type: application/json' \
    -d "{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}" \
    "$API/api/v1/auth/login")
  check "login ($code)" "$([ "$code" = 200 ] && echo 0 || echo 1)"
  # Prove the jar actually holds the session before anything downstream blames the API.
  if grep -q omnion_session "$JAR" 2>/dev/null; then
    check "the session cookie was stored" 0
  else
    check "the session cookie was stored" 1
    echo "     (the jar is empty; every request below would fail with organization_required)"
  fi
}

# A tiny JSON field read without jq: prints the value of a top-level string key.
field() { grep -o "\"$2\"[[:space:]]*:[[:space:]]*\"[^\"]*\"" "$1" | head -1 | sed 's/.*:[[:space:]]*"//; s/"$//'; }

echo "== REQ-087 slice 2 · credential contract =="
login

echo "-- create"
body="{\"name\":\"QA fixture $STAMP\",\"type\":\"api_key\",\"settings\":{\"header\":\"X-Key\"}}"
code=$(curl -s -o /tmp/qa-create.json -w '%{http_code}' -b "$JAR" \
  -H 'content-type: application/json' -d "$body" "$API/api/v1/credentials")
check "create returns 201 (got $code)" "$([ "$code" = 201 ] && echo 0 || echo 1)"
ID=$(grep -o '"id"[[:space:]]*:[[:space:]]*"[^"]*"' /tmp/qa-create.json | head -1 | sed 's/.*:[[:space:]]*"//; s/"$//')
KEY=$(field /tmp/qa-create.json key)
echo "     id=$ID key=$KEY"
check "the response has an id" "$([ -n "$ID" ] && echo 0 || echo 1)"
check "the key is derived from the name" "$([ "$KEY" = "qa-fixture-$STAMP" ] && echo 0 || echo 1)"
check "no secret in the create response" "$(grep -q "$FIXTURE" /tmp/qa-create.json && echo 1 || echo 0)"
check "no handle in the create response" "$(grep -q 'secret_ref\|vault://' /tmp/qa-create.json && echo 1 || echo 0)"
check "has_secret is a boolean, not a value" "$(grep -q '"has_secret":false' /tmp/qa-create.json && echo 0 || echo 1)"
check "the non-secret setting round-trips" "$(grep -q '"header":"X-Key"' /tmp/qa-create.json && echo 0 || echo 1)"

echo "-- a secret inside settings is refused by name"
code=$(curl -s -o /tmp/qa-secretin.json -w '%{http_code}' -b "$JAR" \
  -H 'content-type: application/json' \
  -d "{\"name\":\"leak $STAMP\",\"type\":\"api_key\",\"settings\":{\"api_key\":\"$FIXTURE\"}}" \
  "$API/api/v1/credentials")
CODE=$(field /tmp/qa-secretin.json code)
check "create with a secret in settings → 400 (got $code)" "$([ "$code" = 400 ] && echo 0 || echo 1)"
check "  …with credential_secret_write_only (got $CODE)" "$([ "$CODE" = credential_secret_write_only ] && echo 0 || echo 1)"
check "  …naming the field" "$(grep -q 'api_key' /tmp/qa-secretin.json && echo 0 || echo 1)"
check "  …and not echoing the value" "$(grep -q "$FIXTURE" /tmp/qa-secretin.json && echo 1 || echo 0)"

echo "-- a secret re-sent on PATCH is refused, and nothing changes"
code=$(curl -s -o /tmp/qa-patch.json -w '%{http_code}' -b "$JAR" -X PATCH \
  -H 'content-type: application/json' \
  -d "{\"name\":\"renamed $STAMP\",\"secrets\":[{\"field\":\"api_key\",\"value\":\"$FIXTURE\"}]}" \
  "$API/api/v1/credentials/$ID")
CODE=$(field /tmp/qa-patch.json code)
check "PATCH with a secret → 400 (got $code)" "$([ "$code" = 400 ] && echo 0 || echo 1)"
check "  …with credential_secret_write_only (got $CODE)" "$([ "$CODE" = credential_secret_write_only ] && echo 0 || echo 1)"
check "  …naming the replace path" "$(grep -q '/secret' /tmp/qa-patch.json && echo 0 || echo 1)"
# The refusal happens before the rename, so the old name is still there: a refused write with a
# side effect is worse than no write at all.
code=$(curl -s -o /tmp/qa-after.json -b "$JAR" "$API/api/v1/credentials/$ID")
check "the rename did NOT happen" "$(grep -q "renamed $STAMP" /tmp/qa-after.json && echo 1 || echo 0)"

echo "-- the test hook reports a result, not a pass"
code=$(curl -s -o /tmp/qa-test.json -w '%{http_code}' -b "$JAR" -X POST "$API/api/v1/credentials/$ID/test")
OK=$(grep -o '"ok"[[:space:]]*:[[:space:]]*[a-z]*' /tmp/qa-test.json | head -1 | sed 's/.*:[[:space:]]*//')
HEALTH=$(field /tmp/qa-test.json health)
check "test → 200 (got $code)" "$([ "$code" = 200 ] && echo 0 || echo 1)"
check "  …reports ok:false (got $OK)" "$([ "$OK" = false ] && echo 0 || echo 1)"
check "  …leaves the row untested (got $HEALTH)" "$([ "$HEALTH" = untested ] && echo 0 || echo 1)"
check "  …and explains itself" "$(grep -qi 'replace secret' /tmp/qa-test.json && echo 0 || echo 1)"
check "  …without echoing the secret" "$(grep -q "$FIXTURE" /tmp/qa-test.json && echo 1 || echo 0)"

echo "-- usage on an unreferenced credential"
code=$(curl -s -o /tmp/qa-usage.json -w '%{http_code}' -b "$JAR" "$API/api/v1/credentials/$ID/usage")
INUSE=$(grep -o '"in_use"[[:space:]]*:[[:space:]]*[a-z]*' /tmp/qa-usage.json | head -1 | sed 's/.*:[[:space:]]*//')
check "usage → 200 (got $code)" "$([ "$code" = 200 ] && echo 0 || echo 1)"
check "  …reports in_use:false (got $INUSE)" "$([ "$INUSE" = false ] && echo 0 || echo 1)"
check "  …echoes the key it is about" "$(grep -q "\"$KEY\"" /tmp/qa-usage.json && echo 0 || echo 1)"

echo "-- an unknown filter is a refusal naming the legal set"
code=$(curl -s -o /tmp/qa-filter.json -w '%{http_code}' -b "$JAR" "$API/api/v1/credentials?type=carrier_pigeon")
CODE=$(field /tmp/qa-filter.json code)
check "?type=carrier_pigeon → 400 (got $code)" "$([ "$code" = 400 ] && echo 0 || echo 1)"
check "  …with credential_type_unknown (got $CODE)" "$([ "$CODE" = credential_type_unknown ] && echo 0 || echo 1)"
check "  …and lists api_key" "$(grep -q 'api_key' /tmp/qa-filter.json && echo 0 || echo 1)"

echo "-- an unknown health value is refused the same way"
code=$(curl -s -o /tmp/qa-health.json -w '%{http_code}' -b "$JAR" "$API/api/v1/credentials?health=purple")
CODE=$(field /tmp/qa-health.json code)
check "?health=purple → 400 (got $code)" "$([ "$code" = 400 ] && echo 0 || echo 1)"
check "  …with credential_health_unknown (got $CODE)" "$([ "$CODE" = credential_health_unknown ] && echo 0 || echo 1)"

echo "-- the guard allows what nothing references"
code=$(curl -s -o /tmp/qa-del.json -w '%{http_code}' -b "$JAR" -X DELETE "$API/api/v1/credentials/$ID")
DELETED=$(grep -o '"deleted"[[:space:]]*:[[:space:]]*[a-z]*' /tmp/qa-del.json | head -1 | sed 's/.*:[[:space:]]*//')
check "delete → 200 (got $code)" "$([ "$code" = 200 ] && echo 0 || echo 1)"
check "  …and reports deleted:true (got $DELETED)" "$([ "$DELETED" = true ] && echo 0 || echo 1)"
code=$(curl -s -o /dev/null -w '%{http_code}' -b "$JAR" "$API/api/v1/credentials/$ID")
check "the row is gone (404 expected, got $code)" "$([ "$code" = 404 ] && echo 0 || echo 1)"

echo "-- the package ledger"
code=$(curl -s -o /tmp/qa-pk.json -w '%{http_code}' -b "$JAR" "$API/api/v1/node-packages")
check "GET /node-packages → 200 (got $code)" "$([ "$code" = 200 ] && echo 0 || echo 1)"
code=$(curl -s -o /tmp/qa-pki.json -w '%{http_code}' -b "$JAR" -X POST \
  -H 'content-type: application/json' \
  -d "{\"key\":\"qa-pkg-$STAMP\",\"version\":\"1.0.0\",\"source\":\"local\",\"checksum\":\"abc123\",\"permissions\":[]}" \
  "$API/api/v1/node-packages")
check "POST /node-packages → 201 (got $code)" "$([ "$code" = 201 ] && echo 0 || echo 1)"
# An install with a bad source is refused rather than recorded.
code=$(curl -s -o /tmp/qa-pkbad.json -w '%{http_code}' -b "$JAR" -X POST \
  -H 'content-type: application/json' \
  -d "{\"key\":\"qa-pkg-bad-$STAMP\",\"version\":\"1.0.0\",\"source\":\"smuggled\",\"checksum\":\"abc\",\"permissions\":[]}" \
  "$API/api/v1/node-packages")
check "a package with an unknown source → 400 (got $code)" "$([ "$code" = 400 ] && echo 0 || echo 1)"
code=$(curl -s -o /tmp/qa-pkdel.json -w '%{http_code}' -b "$JAR" -X DELETE "$API/api/v1/node-packages/qa-pkg-$STAMP")
check "DELETE /node-packages/{key} → 204 (got $code)" "$([ "$code" = 204 ] && echo 0 || echo 1)"

echo "-- the schema has no secret column at all"
# A structural claim, checked against the live database rather than the migration text: if a
# future migration adds `api_key` to this table, this is the check that notices.
cols=$(PGPASSWORD=omnion psql -h 127.0.0.1 -p 5433 -U omnion -d "${QA_DB:-omnion_qa_w10}" -tAc \
  "select string_agg(column_name, ',') from information_schema.columns where table_name = 'workflow_credentials'" 2>/dev/null)
echo "     columns: $cols"
for forbidden in api_key token password secret client_secret; do
  case ",$cols," in
    *",$forbidden,"*) check "no \`$forbidden\` column" 1 ;;
    *) check "no \`$forbidden\` column" 0 ;;
  esac
done
check "the table exists" "$(echo "$cols" | grep -q secret_ref && echo 0 || echo 1)"

rm -f "$JAR" /tmp/qa-*.json
echo
echo "== $pass passed, $fail failed =="
[ "$fail" = 0 ]
