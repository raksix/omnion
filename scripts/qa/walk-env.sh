#!/usr/bin/env bash
# Source-only: exports the environment `scripts/qa/run.sh` builds for a pass, so a single walk can
# be driven against the same stack the pass uses.
#
# Why this file exists: the database password is a build fact of the QA container, not something a
# human should copy out of a shell into a command line. Reading it from `run.sh` keeps one source
# of truth — and, more importantly, keeps the credential out of argv, where it would otherwise
# land in the shell history and in every `ps` on a box seven writers share.
#
#   source scripts/qa/walk-env.sh omnion_test_w2tz    # a throwaway database for one walk
#   source scripts/qa/walk-env.sh                     # or the stack's own database
set -euo pipefail
WALK_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

_walk_url() { # database
  python3 - "$WALK_ROOT" "$1" <<'PY'
import re, sys
root, database = sys.argv[1], sys.argv[2]
src = open(f"{root}/scripts/qa/run.sh").read()
template = re.search(r'QA_DATABASE_URL="(postgres://[^"]+)"', src).group(1)

# `run.sh` builds its URL from shell variables, so the literal text is
#   postgres://user:pass@127.0.0.1:${QA_PG_PORT:-$DEFAULT_PG_PORT}/$QA_DB_NAME
# and BOTH references have to go. Three attempts at a regex are recorded here because each failed
# in a way that looked fine: `[^}]*` stops at the *inner* `}` of the nested default and leaves half
# a reference behind; a "resolve until stable" loop needs two passes to get there; and stripping
# braces before the guard throws the reference away before anything can complain. So the names are
# substituted from a table this file owns, and the leftover guard is the part that makes it safe:
# anything still unresolved is a hard error, never a URL.
values = {"QA_DB_NAME": database, "QA_PG_PORT": "5433", "DEFAULT_PG_PORT": "5433"}

def substitute(text):
    def one(match):
        name, default = match.group(1), match.group(3)
        if name in values:
            return values[name]
        return default if default else ""
    # `${NAME:-default}` and `$NAME`, innermost-first so the nested default resolves before its
    # wrapper. The innermost pattern has no braces inside it, which is what `[^}]*` gets wrong.
    text = re.sub(r"\$([A-Za-z_][A-Za-z0-9_]*)(?!:?-)", lambda m: values.get(m.group(1), ""), text)
    text = re.sub(r"\$\{([A-Za-z_][A-Za-z0-9_]*)(:-([^}]*))?\}", one, text)
    return text

resolved = substitute(template)
leftover = re.findall(r"\$\{[^}]*\}|\$[A-Za-z_][A-Za-z0-9_]*", resolved)
if leftover:
    raise SystemExit(f"the URL template still holds unresolved references: {leftover}")
if not resolved.startswith("postgres://") or "@" not in resolved:
    raise SystemExit(f"the URL template did not parse as a connection string: {resolved!r}")
print(resolved)
PY
}

export OMNION_DATABASE_URL="$(_walk_url "${1:-omnion_qa_w2}")"
export OMNION_REDIS_URL="${OMNION_REDIS_URL:-redis://127.0.0.1:6379}"
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/mnt/apopic/omnion-w2-target}"
export CARGO_INCREMENTAL=0