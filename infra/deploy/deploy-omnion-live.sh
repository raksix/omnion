#!/usr/bin/env bash
# Omnion — deploy the live development instance (omnion.fermag.com.tr).
#
#   bash infra/deploy/deploy-omnion-live.sh
#
# Pulls origin/main into the deploy worktree, rebuilds the API and both web apps, recreates the
# pm2 processes with the environment from $OMNION_LIVE_ENV and reports health.
set -euo pipefail

ENV_FILE="${OMNION_LIVE_ENV:-/root/.omnion-live.env}"
WORKTREE="${OMNION_LIVE_DIR:-/mnt/apopic/omnion-live}"
HOST="${OMNION_LIVE_HOST:-omnion.fermag.com.tr}"

[ -f "$ENV_FILE" ] || { echo "[deploy] missing environment file: $ENV_FILE" >&2; exit 1; }
set -a
# shellcheck disable=SC1090
. "$ENV_FILE"
set +a

cd "$WORKTREE"
git fetch origin -q
git checkout -q --detach origin/main
echo "[deploy] revision: $(git log --oneline -1)"

export PATH="$HOME/.cargo/bin:$PATH"
echo "[deploy] building the API (release)"
cargo build --release -p omnion-api -p omnion-cli

STERILE=(env -i "HOME=$HOME" "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" "OMNION_API_URL=$OMNION_API_URL")
echo "[deploy] installing workspace dependencies"
"${STERILE[@]}" pnpm install --frozen-lockfile
echo "[deploy] building the admin panel and the public renderer"
"${STERILE[@]}" pnpm --filter @omnion/admin build
"${STERILE[@]}" pnpm --filter @omnion/web build

echo "[deploy] recreating the pm2 processes"
pm2 delete omnion-api omnion-admin omnion-web >/dev/null 2>&1 || true
pm2 start "$WORKTREE/target/release/omnion-api" --name omnion-api --time >/dev/null
OMNION_API_URL="$OMNION_API_URL" pm2 start "$WORKTREE/apps/admin/node_modules/next/dist/bin/next" \
  --name omnion-admin --cwd "$WORKTREE/apps/admin" --time -- start --port "${OMNION_ADMIN_PORT:-3180}" --hostname 127.0.0.1 >/dev/null
OMNION_API_URL="$OMNION_API_URL" pm2 start "$WORKTREE/apps/web/node_modules/next/dist/bin/next" \
  --name omnion-web --cwd "$WORKTREE/apps/web" --time -- start --port "${OMNION_WEB_PORT:-3280}" --hostname 127.0.0.1 >/dev/null
pm2 save >/dev/null

sleep 5
echo "[deploy] health"
# The probe contract, and the reason the two lines are not one (REQ-126, slice 4):
#
#   /healthz is LIVENESS. It answers "is the process up" and stays 200 through a drain, because
#   a liveness check that fails while the process is shutting down exactly as asked restarts it.
#   /readyz is READINESS. It answers "should traffic be sent here" and flips to 503 the instant
#   a drain begins — before the listener stops accepting, so a proxy polling in the gap is told to
#   stop rather than to keep sending into a socket that is about to close.
#
# A deploy that only curls /healthz proves the process started. One that curls /readyz proves it
# can serve. Both, in that order, and neither used as a substitute for the other.
curl -fsS "http://127.0.0.1:${OMNION_PORT:-4180}/healthz" && echo
# Bounded, because a readiness probe that hangs is a deploy that hangs: this is the one call in
# the script with a timeout, and it is the one that talks to the database.
curl -fsS --max-time 10 "http://127.0.0.1:${OMNION_PORT:-4180}/readyz" && echo
curl -s -o /dev/null -w "[deploy] panel  https://%{http_code}\n" "https://$HOST/login"
curl -s -o /dev/null -w "[deploy] renderer https://%{http_code}\n" "https://demo.$HOST/"

# The metrics endpoint is unauthenticated on the internal interface, so the deploy checks it the
# same way it checks the probes: if the exposition is empty, the registry is not recording and
# every dashboard in `infra/observability/` is about to show a flat line that looks like a quiet
# system. A non-empty scrape is a one-line check for a whole class of silent monitoring failure.
FAMILIES=$(curl -fsS --max-time 5 "http://127.0.0.1:${OMNION_PORT:-4180}/metrics" \
  | grep -c '^# TYPE omnion_' || true)
echo "[deploy] metric families declared: $FAMILIES"
if [ "${FAMILIES:-0}" -lt 20 ]; then
  echo "[deploy] WARNING: the exposition declares $FAMILIES families; the bundle's dashboards expect" \
       "more than 20. The instance is up but its monitoring is not." >&2
fi

# The shutdown knobs, stated where the operator deploying will read them. `OMNION_DRAIN_TIMEOUT_MS`
# has to fit inside pm2's kill timeout (default 1600ms in this ecosystem) with room for the
# telemetry flush that follows, or a deploy cuts requests and loses the last batch.
echo "[deploy] drain timeout: ${OMNION_DRAIN_TIMEOUT_MS:-10000}ms (raise pm2's kill_timeout to match)"
