#!/usr/bin/env bash
# Unblock 127.0.0.1 for sign-in on the QA database, and report what was there.
#
# Why this exists: the security policy refuses an address after a run of failed sign-ins
# (`crates/identity/src/signin.rs`, `reason: "address_failures"`), and `credential-contract.sh`
# logs in on every run. After a handful of runs the failures pile up against 127.0.0.1 and every
# request answers `address_blocked` — which reads exactly like an authorization bug and is
# really a probe that locked itself out. The browser pass logs in once per pass and never trips
# it; a contract probe is the thing that needs the repair, which is why it lives next to it.
#
# The column is `ip_address`, not `address`. Getting that wrong makes the `delete` match no rows
# and the script still prints a cheerful "after: 0" — so the script *proves* the unblock with a
# real login instead of trusting its own count.
#
# QA-fixture only: the database name is checked and a non-QA database is refused.
set -uo pipefail
DB="${QA_DB:-omnion_qa_w10}"
case "$DB" in
  omnion_qa*) : ;;
  *) echo "refusing: $DB is not a QA database"; exit 1 ;;
esac

before=$(PGPASSWORD=omnion psql -h 127.0.0.1 -p 5433 -U omnion -d "$DB" -tAc \
  "select count(*) from sign_in_attempts where ip_address = '127.0.0.1'" 2>/dev/null || echo 0)
echo "sign-in attempts recorded for 127.0.0.1: $before"

PGPASSWORD=omnion psql -h 127.0.0.1 -p 5433 -U omnion -d "$DB" -q -c \
  "delete from sign_in_attempts where ip_address = '127.0.0.1'" >/dev/null 2>&1

after=$(PGPASSWORD=omnion psql -h 127.0.0.1 -p 5433 -U omnion -d "$DB" -tAc \
  "select count(*) from sign_in_attempts where ip_address = '127.0.0.1'" 2>/dev/null || echo 0)
echo "after: $after"

# Prove it with a real login rather than trusting the count above.
API="${API:-http://127.0.0.1:18089}"
code=$(curl -s -o /tmp/unblock-check.json -w '%{http_code}' -c /tmp/unblock.jar -X POST \
  -H 'content-type: application/json' \
  -d '{"email":"qa-owner@omnion.test","password":"OmnionQa-Passw0rd-2026!"}' \
  "$API/api/v1/auth/login")
echo "login now: $code $(head -c 120 /tmp/unblock-check.json)"
rm -f /tmp/unblock.jar /tmp/unblock-check.json
[ "$code" = 200 ]
