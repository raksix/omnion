#!/usr/bin/env bash
#
# The events + webhooks walk (docs/BUILD-BACKLOG.md P12).
#
# It drives a running API and a running receiver (infra/mocks/webhook-receiver.mjs) end to end:
# sign in, open a tenant with a site and a page, connect the receiver as an endpoint subscribed
# to `page.published`, publish the page, and wait until the receiver reports a delivery whose
# signature verifies — then read the delivery back out of the platform's own queue history.
#
# Everything it needs comes from the environment:
#
#   OMNION_API             API base URL          (default http://127.0.0.1:8082)
#   OMNION_RECEIVER        receiver base URL     (default http://127.0.0.1:8124)
#   OMNION_WEBHOOK_SECRET  signing secret        (default omnion-dev-webhook-secret)
#   OMNION_ADMIN_EMAIL     the bootstrapped administrator (default admin@example.com)
#   OMNION_ADMIN_PASSWORD  its password (required)
#
# The walk is idempotent in the sense that every run opens its own tenant, page and endpoint —
# nothing it wrote is reused or removed.
set -euo pipefail

API=${OMNION_API:-http://127.0.0.1:8082}
RECEIVER=${OMNION_RECEIVER:-http://127.0.0.1:8124}
SECRET=${OMNION_WEBHOOK_SECRET:-omnion-dev-webhook-secret}
EMAIL=${OMNION_ADMIN_EMAIL:-admin@example.com}
PASSWORD=${OMNION_ADMIN_PASSWORD:?OMNION_ADMIN_PASSWORD must be set}
STAMP=$(date +%s)

JAR=$(mktemp)
trap 'rm -f "$JAR"' EXIT

json() {
  python3 -c "import json,sys; print(json.load(sys.stdin)$1)"
}

echo "webhooks walk: api=$API receiver=$RECEIVER"

# 1. The administrator signs in.
curl -fsS -c "$JAR" -H 'content-type: application/json' \
  -d "{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}" \
  "$API/api/v1/auth/login" > /dev/null

# 2. A tenant, a site and a page.
org_id=$(curl -fsS -b "$JAR" -H 'content-type: application/json' \
  -d "{\"name\":\"Webhook Walk $STAMP\",\"slug\":\"webhook-walk-$STAMP\"}" \
  "$API/api/v1/organizations" | json "['id']")
site_id=$(curl -fsS -b "$JAR" -H 'content-type: application/json' \
  -d "{\"organization_id\":\"$org_id\",\"key\":\"main\",\"name\":\"Webhook Walk Site\"}" \
  "$API/api/v1/sites" | json "['id']")
page_id=$(curl -fsS -b "$JAR" -H 'content-type: application/json' \
  -d "{\"site_id\":\"$site_id\",\"slug\":\"home\",\"title\":\"Webhook walk home\"}" \
  "$API/api/v1/pages" | json "['id']")

# 3. The receiver becomes an endpoint. The secret is supplied by the operator here, so both
#    sides sign and verify with the same value (and the API never echoes it back).
endpoint_id=$(curl -fsS -b "$JAR" -H 'content-type: application/json' \
  -d "{\"organization_id\":\"$org_id\",\"name\":\"Walk Receiver\",\"url\":\"$RECEIVER/hooks/omnion\",\"events\":[\"page.published\"],\"secret\":\"$SECRET\"}" \
  "$API/api/v1/webhooks" | json "['id']")

if curl -fsS -b "$JAR" "$API/api/v1/webhooks" | grep -q "$SECRET"; then
  echo "the stored secret must never come back through the API" >&2
  exit 1
fi

# 4. Publishing the page records `page.published`; the delivery runner posts it.
curl -fsS -b "$JAR" -X POST "$API/api/v1/pages/$page_id/publish" > /dev/null

delivered=""
for _ in $(seq 1 30); do
  delivered=$(curl -fsS "$RECEIVER/received" | python3 -c "
import json,sys
rows=[r for r in json.load(sys.stdin)['received'] if r['event']=='page.published']
print(json.dumps(rows[-1]) if rows else '')
")
  if [ -n "$delivered" ]; then
    break
  fi
  sleep 1
done

if [ -z "$delivered" ]; then
  echo "no page.published delivery reached the receiver within 30s" >&2
  exit 1
fi

# 5. The signature must verify against the bytes that arrived, and the body must be the event.
python3 - "$delivered" "$SECRET" <<'PY'
import json, sys

delivery = json.loads(sys.argv[1])
secret_chars = len(sys.argv[2])

assert delivery["signature_valid"] is True, delivery
assert delivery["event"] == "page.published", delivery
assert delivery["delivery"], "the delivery header must carry an id"
assert delivery["body"]["name"] == "page.published", delivery
assert delivery["body"]["payload"]["slug"] == "home", delivery
assert delivery["body"]["payload"]["title"] == "Webhook walk home", delivery
assert secret_chars >= 16, "the receiver signs with a real secret"
print("receiver: event=%s signature=verified bytes=%d slug=%s" % (
    delivery["event"], delivery["bytes"], delivery["body"]["payload"]["slug"]))
PY

# 6. The platform's own queue history agrees.
curl -fsS -b "$JAR" "$API/api/v1/webhooks/$endpoint_id/deliveries" | python3 -c "
import json,sys
rows=json.load(sys.stdin)['deliveries']
assert rows, 'the endpoint must have a delivery'
row=rows[0]
assert row['status']=='delivered', row
assert row['response_status']==200, row
assert row['event_name']=='page.published', row
print('platform: status=%s attempts=%d response=%s event=%s' % (
    row['status'], row['attempts'], row['response_status'], row['event_name']))
"

# 7. The event is on the bus, and the operator's action is in the audit trail.
curl -fsS -b "$JAR" "$API/api/v1/events?name=page.published" | python3 -c "
import json,sys
rows=json.load(sys.stdin)['events']
assert any(r['payload'].get('slug')=='home' for r in rows), rows
print('bus: %d page.published event(s) on the feed' % len(rows))
"

echo "webhooks walk: OK — signed delivery verified end to end"
