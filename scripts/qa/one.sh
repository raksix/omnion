#!/usr/bin/env bash
# One test from a gate, with the gate's own environment. Used when a failure needs its `left:` /
# `right:` pair without re-running the whole suite.
#   bash scripts/qa/one.sh project_switcher a_project_outside
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR="${QA_CARGO_TARGET_DIR:-/mnt/apopic/w8build}"
export CARGO_INCREMENTAL=0
# A writable TMPDIR: this box's root filesystem is regularly at 100% and a `cc` build step that
# cannot write its own temp file dies with "No space left on device", which reads as a compile
# failure of the product.
export TMPDIR="${CARGO_TARGET_DIR}/tmp"
mkdir -p "$TMPDIR"

SUITE="${1:?suite name}"
shift

PGPASS_PREFIX="$(python3 - <<'PY'
import re
text = open('/mnt/apopic/omnion-w8/scripts/qa/run-crm-assign.sh', encoding='utf-8').read()
m = re.search(r'DATABASE_URL="(postgres://[^@"\[\]]+@127\.0\.0\.1:5433)/', text)
print(m.group(1) if m else '')
PY
)"
export DATABASE_URL="${PGPASS_PREFIX}/omnion_qa_w8_switcher"

cargo test -p omnion-workflows --test "$SUITE" -- --test-threads=1 --nocapture "$@"
