#!/usr/bin/env bash
# The SDK gate for REQ-130 slice 3 — acceptance line 10, in full.
#
#   bash scripts/qa/sdk-gate.sh [--base-url http://127.0.0.1:18085]
#
# Acceptance 10 says: *"SDKs generate identical output from a pinned hash, and both packages
# compile and pass smoke tests against a live test server."* Four claims, four commands, and the
# order matters — a later one is worth less if an earlier one failed.
#
#   1. the document is in sync with the router      (openapi_emit --check)
#   2. generation is reproducible from one pin      (two runs, byte-identical)
#   3. both packages compile in their toolchain     (inside the crate's own test)
#   4. both clients complete a real call            (against a live API)
#
# **Why 4 needs a server and not a mock.** The generator and its client agree by construction:
# the client builds a URL from the same document the generator read, so a mock that echoes the
# document would agree with a client that builds the wrong URL. Only the router can say whether
# `/healthz` is served. Every defect this gate found — a reserved method name, a group outside a
# class, a type annotation emitted as a value, an unimported binding — was invisible to a test
# over the generator's own functions and obvious the moment a real import ran.
#
# The API is started only if nothing answers on the port, and it is started on THIS worktree's
# own port and database. It never touches another writer's stack.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH:/usr/local/bin:$PATH"

BASE_URL="${OMNION_SDK_BASE_URL:-http://127.0.0.1:18085}"
while [ $# -gt 0 ]; do
  case "$1" in
    --base-url) BASE_URL="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

STACK="w6"
DB="omnion_qa_w${STACK#w}"
API_NAME="omnion-qa-api-$STACK"
API_PORT="${BASE_URL##*:}"
API_BIN="$(readlink -f target)/debug/omnion-api"

step() { printf '\n[sdk-gate] %s\n' "$*"; }

step "1/4 the document is in sync with the router"
cargo run -q -p omnion-api --bin openapi_emit -- --check

step "2/4 generation is reproducible from one pin"
HASH="$(cargo run -q -p omnion-api --bin sdk_emit -- --out /tmp/sdk-a 2>&1 | sed -n 's/^sdk-emit: hash //p')"
[ -n "$HASH" ] || { echo "[sdk-gate] could not read the document hash" >&2; exit 1; }
cargo run -q -p omnion-api --bin sdk_emit -- --hash "$HASH" --out /tmp/sdk-b >/dev/null
for language in typescript python; do
  diff -r "/tmp/sdk-a/$language" "/tmp/sdk-b/$language" >/dev/null \
    || { echo "[sdk-gate] $language is not byte-identical across two runs" >&2; exit 1; }
  echo "  $language: identical across two runs from $HASH"
done
rm -rf /tmp/sdk-a /tmp/sdk-b

step "3/4 both packages compile in their own toolchain (and the whole crate is green)"
cargo test -p omnion-graphql --lib --quiet

step "4/4 both clients complete a real call against $BASE_URL"
if ! curl -fsS --max-time 5 "$BASE_URL/healthz" >/dev/null 2>&1; then
  echo "[sdk-gate] no API on $BASE_URL — starting this worktree's own"
  if [ ! -x "$API_BIN" ]; then
    cargo build -q -p omnion-api --bin omnion-api
  fi
  pm2 delete "$API_NAME" >/dev/null 2>&1 || true
  OMNION_DATABASE_URL="postgres://omnion:omnion@127.0.0.1:5433/$DB" \
  OMNION_REDIS_URL="redis://127.0.0.1:6380" \
  OMNION_PORT="$API_PORT" \
  OMNION_ENV=development \
  OMNION_CSRF_SECRET="sdk-gate-throwaway-not-a-real-key" \
    pm2 start "$API_BIN" --name "$API_NAME" --time >/dev/null
  for _ in $(seq 1 45); do
    curl -fsS --max-time 5 "$BASE_URL/healthz" >/dev/null 2>&1 && break
    sleep 2
  done
fi
curl -fsS --max-time 5 "$BASE_URL/healthz" >/dev/null || {
  echo "[sdk-gate] the API never answered on $BASE_URL" >&2
  pm2 logs "$API_NAME" --lines 20 --nostream || true
  exit 1
}

cargo run -q -p omnion-api --bin sdk_emit >/dev/null
python3 scripts/qa/sdk-smoke.py --base-url "$BASE_URL"
bun run scripts/qa/sdk-smoke.mjs --base-url "$BASE_URL"

printf '\n[sdk-gate] 4/4 green — the document is in sync, generation is reproducible, both\n'
printf '[sdk-gate] packages compile, and both clients talk to a live API.\n'
