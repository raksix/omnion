#!/usr/bin/env bash
# Omnion — a disposable database for one integration walk.
#
# `cargo test --test media_*` connects to whatever OMNION_DATABASE_URL names, and the shared
# development database is not a safe place to run them: a sibling wave's branch has already
# applied migrations this branch does not have, and the suite dies with `VersionMissing(19)`
# before it reaches a single assertion. A suite that reports "the database is wrong" instead
# of "the code is wrong" is a suite that cannot be trusted to have been run at all.
#
# So each suite gets its own database, created fresh and dropped afterwards. It is named after
# the *test target*, which is also why the name is derived here rather than in the test: two
# suites in one crate run as separate processes and would otherwise fight over one name.
#
# Never point this at the development database or at `omnion_qa` — the QA stack resets that one
# on every pass, and a walk that is dropped mid-run reports a migration error that has nothing
# to do with the code.
set -euo pipefail

TARGET="${1:?usage: run-media-walk.sh <test-target> [test name filter ...]}"
shift || true
CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="omnion_walk_${TARGET//-/_}"

docker exec "$CONTAINER" psql -U omnion -d postgres -v ON_ERROR_STOP=1 \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null

export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_INCREMENTAL=0
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
export OMNION_DATABASE_URL="postgres://omnion:omnion@127.0.0.1:5433/${DB}"
export OMNION_REDIS_URL="redis://127.0.0.1:6380"
export OMNION_ENV=development

set +e
cargo test -p omnion-api --test "$TARGET" -- --test-threads=1 "$@"
STATUS=$?
set -e

docker exec "$CONTAINER" psql -U omnion -d postgres -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" >/dev/null 2>&1 || true
exit "$STATUS"
