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
curl -fsS "http://127.0.0.1:${OMNION_PORT:-4180}/healthz" && echo
curl -fsS "http://127.0.0.1:${OMNION_PORT:-4180}/readyz" && echo
curl -s -o /dev/null -w "[deploy] panel  https://%{http_code}\n" "https://$HOST/login"
curl -s -o /dev/null -w "[deploy] renderer https://%{http_code}\n" "https://demo.$HOST/"
