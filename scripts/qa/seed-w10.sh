#!/usr/bin/env bash
# Re-seed the QA w10 stack: owner, organization, site, and the user's organization binding.
# scripts/qa/reset-db.sh drops the database, so every pass that restarts the API has to put
# the fixture back before any authenticated endpoint will answer.
set -uo pipefail
API="${API:-http://127.0.0.1:18089}"
DB="${QA_DB:-omnion_qa_w10}"
EMAIL="qa-owner@omnion.test"
PASSWORD="OmnionQa-Passw0rd-2026!"
JAR=$(mktemp)

j() { curl -s -c "$JAR" -b "$JAR" -H 'content-type: application/json' "$@"; }

j -X POST -d "{\"display_name\":\"QA Owner\",\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}" \
  "$API/api/v1/onboarding/owner" >/dev/null
j -X POST -d '{"name":"QA Organization","slug":"qa-org"}' \
  "$API/api/v1/onboarding/organization" >/dev/null
j -X POST -d '{"name":"QA Site","key":"main","domain":"qa.omnion.test"}' \
  "$API/api/v1/onboarding/site" >/dev/null

# The onboarding flow does not bind the owner to the organization on its own — the wizard does
# it through the panel. Without this every scoped endpoint answers `organization_required`,
# which reads like an authorization bug and is really a missing fixture row.
PGPASSWORD=omnion psql -h 127.0.0.1 -p 5433 -U omnion -d "$DB" -q -c \
  "update users set organization_id = (select id from organizations where slug = 'qa-org')
   where email = '$EMAIL'" >/dev/null

j -X POST -d "{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}" "$API/api/v1/auth/login" >/dev/null
code=$(j -o /dev/null -w '%{http_code}' "$API/api/v1/credentials")
echo "seeded: /api/v1/credentials -> $code"
rm -f "$JAR"
[ "$code" = 200 ]
