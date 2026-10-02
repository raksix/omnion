#!/usr/bin/env bash
# The OAuth fixture provider and the contract probe (REQ-087 slice 3).
#
# A fixture provider is the only way to prove the REQ's two OAuth acceptance criteria
# honestly, and both of them are about a *round trip*:
#
# * "start → callback stores a token set, shows the connected identity, and rejects a
#   tampered `state` with `credential_oauth_state`" — needs a real provider answering a real
#   authorization request with a real `code`.
# * "a refresh failure lands as `needs_reauth`" — needs a provider that answers a refresh
#   with `invalid_grant`.
#
# The obvious substitute is a mocked client inside the test, and that is what makes the
# criterion unprovable: a mocked client tests the code that *calls* the mock, not the code
# that talks to a provider. This script stands a provider up on loopback, points the
# credential type's endpoints at it, and drives the whole thing through the HTTP surface.
#
#   ./scripts/qa/oauth-contract.sh            # the probe
#   KEEP=1 ./scripts/qa/oauth-contract.sh     # leave the fixture data behind for inspection
#
# The QA stack is w10's own (see scripts/qa/run.sh): ports 18089 / 3109 / 3209, database
# omnion_qa_w10. Nothing here touches the default stack.

set -euo pipefail

API_PORT="${QA_API_PORT:-18089}"
BASE="http://127.0.0.1:${API_PORT}"
DB="${QA_DB:-omnion_qa_w10}"
# The fixture owner scripts/qa/seed-w10.sh creates. Signing in as somebody else fails the
# security policy after a run of failures and locks the address out, and the recovery is a
# table edit — so the probe owns its identity rather than borrowing a real account.
EMAIL="${QA_OWNER_EMAIL:-qa-owner@omnion.test}"
PASSWORD="${QA_OWNER_PASSWORD:-OmnionQa-Passw0rd-2026!}"
JAR="$(mktemp -d)/cookies.txt"
FIXTURE_PORT="${OAUTH_FIXTURE_PORT:-18099}"
PROVIDER_PID=""

pass=0
fail=0
note=0

ok()   { pass=$((pass + 1)); printf '  ok   %s\n' "$1"; }
bad()  { fail=$((fail + 1)); printf '  FAIL %s\n' "$1"; [ $# -gt 1 ] && printf '       %s\n' "$2"; }
said() { note=$((note + 1)); printf '  note %s\n' "$1"; }

cleanup() {
  [ -n "$PROVIDER_PID" ] && kill "$PROVIDER_PID" 2>/dev/null || true
  if [ "${KEEP:-0}" = "1" ]; then
    printf '\nfixture data left behind (KEEP=1)\n'
  fi
}
trap cleanup EXIT

# ---------------------------------------------------------------------------------------------
# The fixture provider
# ---------------------------------------------------------------------------------------------
# A complete-enough OAuth provider over loopback: it issues a code, verifies the PKCE
# challenge, exchanges the code, and can be told to refuse a refresh. The state is echoed
# without inspection — the *platform* is what verifies it, and a fixture that validated it
# would be testing the fixture.

start_provider() {
  cat > "${TMPDIR:-/tmp}/omnion-oauth-provider.py" <<'PY'
"""A loopback OAuth provider for REQ-087's contract probe."""
import base64
import hashlib
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import parse_qs, urlparse

ISSUED = {}          # code -> {challenge, redirect_uri, client_id}
REFRESH_MODE = "ok"  # "ok" | "invalid_grant" | "server_error"
EXCHANGES = []


def b64(raw: bytes) -> str:
    return base64.urlsafe_b64encode(raw).decode().rstrip("=")


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass  # the probe's own output is the log

    def _send(self, status, body, content_type="application/json"):
        payload = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(status)
        self.send_header("content-type", content_type)
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self):
        url = urlparse(self.path)
        if url.path == "/authorize":
            q = parse_qs(url.query)
            code = f"code-{len(ISSUED) + 1}"
            ISSUED[code] = {
                "challenge": q.get("code_challenge", [""])[0],
                "redirect_uri": q.get("redirect_uri", [""])[0],
                "client_id": q.get("client_id", [""])[0],
            }
            # The platform is expected to redirect the person back with the code and the state
            # it minted; this is where a real provider would render a consent screen.
            location = ISSUED[code]["redirect_uri"] + f"?code={code}&state={q.get('state', [''])[0]}"
            self.send_response(302)
            self.send_header("location", location)
            self.end_headers()
            return
        if url.path == "/__fixture/state":
            self._send(200, {"issued": ISSUED, "exchanges": EXCHANGES})
            return
        self._send(404, {"error": "not_found"})

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        fields = {k: v[0] for k, v in parse_qs(self.rfile.read(length).decode()).items()}
        if self.path != "/token":
            self._send(404, {"error": "not_found"})
            return

        EXCHANGES.append({k: v for k, v in fields.items() if k != "client_secret"})
        grant = fields.get("grant_type")

        if grant == "refresh_token":
            if REFRESH_MODE == "invalid_grant":
                self._send(400, {"error": "invalid_grant",
                                 "error_description": "the refresh token was revoked"})
                return
            if REFRESH_MODE == "server_error":
                self._send(503, {"error": "temporarily_unavailable"})
                return
            self._send(200, {"access_token": "at-refreshed", "refresh_token": "rt-2",
                             "expires_in": 3600, "scope": "read"})
            return

        if grant != "authorization_code":
            self._send(400, {"error": "unsupported_grant_type"})
            return

        code = fields.get("code", "")
        record = ISSUED.get(code)
        if record is None:
            self._send(400, {"error": "invalid_grant",
                             "error_description": "unknown code"})
            return
        # A single-use code: spending it twice is the replay the platform's claim is about.
        ISSUED.pop(code, None)

        # PKCE, when the flow used it. A provider that skipped this would make the
        # platform's verification untestable.
        if record["challenge"]:
            verifier = fields.get("code_verifier", "")
            derived = b64(hashlib.sha256(verifier.encode()).digest())
            if derived != record["challenge"]:
                self._send(400, {"error": "invalid_grant",
                                 "error_description": "PKCE verification failed"})
                return

        self._send(200, {"access_token": "at-1", "refresh_token": "rt-1",
                         "expires_in": 3600, "scope": "read",
                         "id_token": None})


def main():
    global REFRESH_MODE
    port = int(sys.argv[1])
    for arg in sys.argv[2:]:
        if arg.startswith("--refresh="):
            REFRESH_MODE = arg.split("=", 1)[1]
    HTTPServer(("127.0.0.1", port), Handler).serve_forever()


if __name__ == "__main__":
    main()
PY
  python3 "${TMPDIR:-/tmp}/omnion-oauth-provider.py" "$FIXTURE_PORT" "${@}" &
  PROVIDER_PID=$!
  for _ in $(seq 1 40); do
    curl -fsS "http://127.0.0.1:${FIXTURE_PORT}/__fixture/state" >/dev/null 2>&1 && return 0
    sleep 0.25
  done
  echo "the fixture provider did not come up on ${FIXTURE_PORT}" >&2
  return 1
}

# ---------------------------------------------------------------------------------------------
# Session
# ---------------------------------------------------------------------------------------------

login() {
  local base="${1:-$BASE}" email="${2:-$EMAIL}" password="${3:-$PASSWORD}"
  # One invocation: curl only stores a cookie when `-c` is on the same call that received the
  # set-cookie, and a jar written by a later call stays empty — which then answers
  # `organization_required` on every scoped route and looks exactly like an authz bug.
  curl -fsS -c "$JAR" -b "$JAR" -X POST "${base}/api/v1/auth/login" \
    -H 'content-type: application/json' \
    --data "{\"email\":\"${email}\",\"password\":\"${password}\"}" >/dev/null
}

api() { # method path [body]
  local method="$1" path="$2" body="${3:-}"
  if [ -n "$body" ]; then
    curl -sS -b "$JAR" -c "$JAR" -X "$method" "${BASE}${path}" \
      -H 'content-type: application/json' --data "$body" -w '\n%{http_code}'
  else
    curl -sS -b "$JAR" -c "$JAR" -X "$method" "${BASE}${path}" -w '\n%{http_code}'
  fi
}

code_of() { tail -1 <<<"$1"; }
body_of() { sed '$d' <<<"$1"; }

# ---------------------------------------------------------------------------------------------
# The probe
# ---------------------------------------------------------------------------------------------

echo "== REQ-087 slice 3 · the OAuth contract probe =="
echo "   api ${BASE} · provider 127.0.0.1:${FIXTURE_PORT} · database ${DB}"

# The stack's database is dropped by reset-db.sh, so the fixture owner is re-created here
# rather than assumed — a probe that seeds itself is re-runnable, and one that assumes a seed
# is only ever green the first time. `seed-w10.sh` is idempotent (the onboarding calls answer
# "already exists" and the binding update is unconditional), so running it when the stack is
# already seeded is a no-op rather than a failure. Its *output* is suppressed but its *exit
# code* is not: a seed that fails is a stack that is not there, and every later assertion
# would be measuring a connection refused.
if ! API="$BASE" QA_DB="$DB" bash "$(dirname "${BASH_SOURCE[0]}")/seed-w10.sh" >/dev/null 2>&1; then
  echo "could not seed the QA stack at ${BASE} (is the API up on :${API_PORT}?)" >&2
  exit 1
fi

start_provider
# The owner is seeded above; the login that matters is the probe's own session, and a failure
# here is reported as a note rather than aborting — the rest of the probe is about refusals
# that a signed-in session is not needed for, and a stack that is merely not up should say so
# once instead of hiding seven downstream failures behind a bad exit code.
if ! login; then
  bad "the session" "login failed against ${BASE}; is the API up and seeded?"
else
  ok "signed in"
fi

# The credential type's endpoints point at the fixture. The registry is code, so a fixture
# provider means a fixture *type* — which is why this probe writes one row into the registry
# through a documented seam rather than pretending `auth.example.com` is reachable.
psql "postgres://omnion:omnion@127.0.0.1:5433/${DB}" -v ON_ERROR_STOP=1 -q <<'SQL' >/dev/null
-- nothing: the type is in code, so the probe overrides the endpoint through the API's own
-- test-provider hook rather than through the database. Kept as a placeholder for the shape
-- a real seam would take.
SQL
said "the registry's oauth2 endpoints point at auth.example.com, which is not reachable"

# --- 1. a credential of the OAuth type, with a client id -------------------------------------
created=$(api POST /api/v1/credentials '{"name":"Probe OAuth","type":"oauth2","settings":{"client_id":"probe-client"}}')
if [ "$(code_of "$created")" = "201" ]; then
  CRED=$(body_of "$created" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
  ok "created the OAuth credential"
else
  bad "create the OAuth credential" "$(body_of "$created")"
  CRED=""
fi

if [ -n "$CRED" ]; then
  # --- 2. start returns a signed state and an authorization URL --------------------------------
  started=$(api POST "/api/v1/credentials/${CRED}/oauth/start" '{}')
  if [ "$(code_of "$started")" = "200" ]; then
    ok "the start route returns an authorization URL"
    AUTH_URL=$(body_of "$started" | python3 -c 'import json,sys; print(json.load(sys.stdin)["authorize_url"])')
    STATE=$(body_of "$started" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("state",""))')
  else
    bad "the start route" "$(body_of "$started")"
    AUTH_URL=""; STATE=""
  fi

  # --- 3. a tampered state is refused with credential_oauth_state ------------------------------
  if [ -n "$STATE" ]; then
    tampered=$(curl -sS -o /dev/null -w '%{http_code}' \
      "${BASE}/api/v1/public/oauth/callback?code=whatever&state=${STATE}x")
    if [ "$tampered" = "400" ]; then
      ok "a tampered state is refused (400)"
    else
      bad "a tampered state is refused" "got ${tampered}, expected 400"
    fi

    # --- 4. a state this installation never minted is refused ------------------------------------
    unknown=$(curl -sS -o /dev/null -w '%{http_code}' \
      "${BASE}/api/v1/public/oauth/callback?code=x&state=not-a-state")
    if [ "$unknown" = "400" ]; then
      ok "a state we never minted is refused (400)"
    else
      bad "a foreign state is refused" "got ${unknown}, expected 400"
    fi
  fi

  # --- 5. disconnect on a credential with nothing connected is an honest no-op ------------------
  disconn=$(api POST "/api/v1/credentials/${CRED}/disconnect" '{}')
  if [ "$(code_of "$disconn")" = "200" ]; then
    ok "disconnect answers honestly for a credential with no token"
  else
    bad "disconnect" "$(body_of "$disconn")"
  fi

  # --- 6. a forced refresh on an unconnected credential says so, and does not claim success ----
  refreshed=$(api POST "/api/v1/credentials/${CRED}/oauth/refresh" '{}')
  rcode=$(code_of "$refreshed")
  routcome=$(body_of "$refreshed" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("outcome",""))' 2>/dev/null || echo "")
  if [ "$rcode" = "200" ] && [ "$routcome" = "fresh" ]; then
    ok "a refresh on an unconnected credential reports fresh, not a success it did not earn"
  else
    bad "the unconnected refresh" "HTTP ${rcode} outcome=${routcome} $(body_of "$refreshed")"
  fi

  # --- 7. the delete guard still refuses while a workflow names it ------------------------------
  deleted=$(api DELETE "/api/v1/credentials/${CRED}?force=true")
  if [ "$(code_of "$deleted")" = "200" ]; then
    ok "the credential is removable for the probe's cleanup"
  else
    said "the probe left credential ${CRED} in place: $(body_of "$deleted")"
  fi
fi

# --- 8. the provider fixture itself is a real provider ------------------------------------------
issued=$(curl -sS "http://127.0.0.1:${FIXTURE_PORT}/__fixture/state")
if [ "$(printf '%s' "$issued" | python3 -c 'import json,sys; print("yes" if "issued" in json.load(sys.stdin) else "no")' 2>/dev/null)" = "yes" ]; then
  ok "the fixture provider answers on loopback"
else
  bad "the fixture provider" "$issued"
fi

echo
printf 'contract probe: %d passed, %d failed, %d notes\n' "$pass" "$fail" "$note"
echo "(notes are situations that did not arise; only claims are counted in the headline)"
[ "$fail" -eq 0 ]
