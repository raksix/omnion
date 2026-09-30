#!/usr/bin/env bash
# Omnion — the image health probe must answer on the endpoint the orchestrator cares about.
#
# The distroless API image has no shell, no `curl` and no `wget`, so its HEALTHCHECK calls the
# binary: `omnion-api --healthcheck`. That makes the flag a deployment contract, and a contract
# with no test is a comment. Three behaviours have to hold, and the third is the one that is easy
# to get wrong:
#
#   * nothing listening        → non-zero, so a container that never bound its port is unhealthy
#   * `/readyz` answering 200  → zero
#   * `/readyz` answering 503  → NON-ZERO
#
# The last is why the probe targets `/readyz` and not `/healthz`. Liveness answers "the process
# exists", so a container whose database is gone probes healthy and a rollout completes on top
# of an instance that cannot serve. Probing readiness makes the orchestrator hold traffic back
# during exactly the window where holding it back is correct.
set -uo pipefail

BIN="${1:-/opt/omnion-w6-target/debug/omnion-api}"
PORT="${2:-18099}"

if [ ! -x "$BIN" ]; then
  printf 'SKIP %s is not an executable binary\n' "$BIN"
  exit 0
fi

pass=0
fail=0
check() { # description, expected, actual
  if [ "$2" = "$3" ]; then
    printf 'ok   %s\n' "$1"; pass=$((pass + 1))
  else
    printf 'FAIL %s\n       expected: %s\n       actual:   %s\n' "$1" "$2" "$3"
    fail=$((fail + 1))
  fi
}

serve() { # <status> -> a one-shot HTTP server answering <status> on $PORT, in the background
  python3 - "$1" "$PORT" <<'PY' &
import sys, http.server
status = int(sys.argv[1])
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        # Only the readiness path is meaningful; anything else is a 404 so a probe pointed at
        # the wrong URL cannot pass by accident.
        code = status if self.path == "/readyz" else 404
        self.send_response(code); self.end_headers()
        self.wfile.write(b'{"ok":true}' if code == 200 else b'{"ok":false}')
    def log_message(self, *a): pass
http.server.HTTPServer(("127.0.0.1", int(sys.argv[2])), H).serve_forever()
PY
  server_pid=$!
  # Give the listener a moment to bind. `sleep` rather than a retry loop: a retry loop would
  # also mask a server that never comes up, and the point of the 503 case is that the probe
  # reads the STATUS, not that it can reach something.
  sleep 1
}

stop_server() { [ -n "${server_pid:-}" ] && kill "$server_pid" 2>/dev/null; wait "$server_pid" 2>/dev/null; server_pid=""; }
trap stop_server EXIT

# 1. Nothing is listening on the port.
OMNION_PORT="$PORT" timeout 20 "$BIN" --healthcheck >/dev/null 2>&1
check "no listener: the probe fails" "1" "$?"

# 2. A ready instance answers 200 and the probe passes.
serve 200
OMNION_PORT="$PORT" timeout 20 "$BIN" --healthcheck >/dev/null 2>&1
check "readyz 200: the probe passes" "0" "$?"
stop_server

# 3. A draining or dependency-broken instance answers 503 and the probe FAILS. This is the
#    behaviour that separates /readyz from /healthz, and it is the one a "just check the port"
#    implementation gets backwards.
serve 503
OMNION_PORT="$PORT" timeout 20 "$BIN" --healthcheck >/dev/null 2>&1
check "readyz 503: the probe fails, so traffic is held back" "1" "$?"
stop_server

# 4. The probe must follow OMNION_PORT, or it drifts onto a different port than the one in use —
#    a container that reports healthy while serving nothing.
serve 200
OMNION_PORT="$((PORT + 1))" timeout 20 "$BIN" --healthcheck >/dev/null 2>&1
check "the probe follows OMNION_PORT rather than a constant" "1" "$?"
stop_server

# 5. `--version` is what a deployment logs to identify an image; it must not boot the server.
out="$(timeout 20 "$BIN" --version 2>&1)"
rc=$?
check "--version exits 0" "0" "$rc"
case "$out" in
  omnion-api\ *) printf 'ok   --version names the service and its version\n'; pass=$((pass + 1)) ;;
  *) printf 'FAIL --version printed %s\n' "$out"; fail=$((fail + 1)) ;;
esac

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
