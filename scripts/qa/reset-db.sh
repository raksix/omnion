#!/usr/bin/env bash
# Omnion QA — reset the dedicated QA database.
#
# The walkthrough always runs against an empty database so that the first-run wizard (and every
# empty state) is exercised on every pass. Never point this at the development database.
#
# It DROPS a database, so its defaults are the part of this script that can destroy somebody
# else's work. It used to default to `omnion_qa` and derive nothing from the stack: a writer whose
# worktree owns a private database (`QA_STACK=w2` → `omnion_qa_w2`) could run this script by hand —
# which is the natural thing to do while debugging a pass — and drop the MAIN writer's database
# instead of its own. `run.sh` exports `QA_DB`, so a pass invoked through the harness was never
# wrong; the hazard was only the direct call, which is exactly the call a human makes. The
# identity of the database is now derived from `QA_STACK` the same way `run.sh` derives it, so the
# two cannot disagree, and an explicit `QA_DB` still wins.
set -euo pipefail

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"

# Same rule as run.sh: `main` keeps the shared name, every other stack owns its own. Deriving it
# here is the point — a copy of the rule is better than a default that names the wrong database.
STACK="${QA_STACK:-main}"
DEFAULT_DB="omnion_qa"
[ "$STACK" != "main" ] && DEFAULT_DB="omnion_qa_${STACK}"
DB="${QA_DB:-$DEFAULT_DB}"

# An explicit override is allowed (run.sh sets it), but a name that is not a QA database is almost
# certainly a typo pointing at a real one — `development`, `omnion`, or a stray trailing character.
# The database this drops is not recoverable from here.
#
# The pattern is a REGEX bracket, not a shell glob: `omnion_qa_*` matches the empty string, so
# `omnion_qa_` — a typo that looks exactly like a QA name — satisfied the glob and was dropped.
# The stack suffix has to be at least one character. This is the same class as the `--only` filter
# that matched nothing and therefore ran everything: a guard that admits the case it was written to
# reject is indistinguishable from no guard at all.
if ! printf '%s' "$DB" | grep -qE '^omnion_qa(_[a-z0-9]+)?$'; then
  echo "[qa] refusing to drop '${DB}': a QA database is 'omnion_qa' or 'omnion_qa_<stack>'." >&2
  exit 2
fi

docker exec "$CONTAINER" psql -U omnion -d postgres -v ON_ERROR_STOP=1 \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null

echo "[qa] ${DB} reset (container ${CONTAINER}, stack ${STACK})"