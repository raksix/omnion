#!/usr/bin/env bash
# CRM intake slice 2 — the simulator's *input* verdict: what a pasted payload carried that
# no rule can read, and the alias a mistyped key was probably meant to be.
#
#   QA_DB=omnion_qa_w8_simulator bash scripts/qa/run-crm-simulator-input.sh
#
# ## Why this is a separate gate from `run-crm-assignment.sh`
#
# That gate proves the *chain*: which rule wins, that the cursor is atomic, that a deadline
# lands inside the business window. All of that is about saved rows and a real transaction.
# This one is about the boundary between what an operator typed and what the evaluator read —
# a boundary with **no database in it at all**, so a database gate is the wrong instrument.
# The interesting failure is `{"contury": "TR"}` reading as a lead with no country, and the
# only thing that can prove that is a payload the reader is handed directly.
#
# ## Why the negative control matters more than the positive
#
# `unread_keys` returning the wrong thing is silent: an empty list looks exactly like a clean
# payload. So the harness re-runs itself with `did_you_mean` forced off and requires the
# three suggestion assertions to go red while the "is unread" assertion stays green. That
# pair is the property, not the suggestion.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR="${QA_CARGO_TARGET_DIR:-/dev/shm/w8-target}"
export CARGO_INCREMENTAL=0

# The module half needs no database. It is pure functions over a pasted jsonb document, and
# a database would prove the wrong thing — that a query runs, not that a key was understood.
cargo test -p omnion-module-crm-intake --lib assignment:: -- --nocapture

echo
echo "[crm-simulator-input] negative control: a reader with no suggestion can only say 'unknown'"

# Force the suggestion off at the one place that decides it, so the property under test is
# the *reporting* of an unread key rather than the cleverness of the guess.
cp modules/crm-intake/src/assignment.rs /tmp/w8-sim-input-backup.rs
python3 - <<'PY'
import re
p = "modules/crm-intake/src/assignment.rs"
s = open(p).read()
needle = "fn closest_alias(key: &str) -> Option<String> {"
assert needle in s, "the gate must find the function it is neutralising"
s = s.replace(needle, needle + "\n    return None; // NEGATIVE CONTROL", 1)
open(p, "w").write(s)
PY

set +e
cargo test -p omnion-module-crm-intake --lib assignment:: -- --nocapture >/tmp/w8-sim-ctl.log 2>&1
control_status=$?
set -e
cp /tmp/w8-sim-input-backup.rs modules/crm-intake/src/assignment.rs
rm -f /tmp/w8-sim-input-backup.rs

if [ "$control_status" -eq 0 ]; then
  echo "  FAIL: neutralising the suggestion left the suite green, so the gate names nothing." >&2
  exit 1
fi

# **The failing set must be exactly the one test.** "Something went red" is the check this
# branch has learned not to accept: an unrelated failure would satisfy a bare exit code, and
# the control would then pass for the wrong reason — which is the defect class this whole gate
# exists to catch. So the names under `failures:` are compared as a set, and any second name
# means the control proved something other than what it claims.
control_failures=$(sed -n '/^failures:$/,$p' /tmp/w8-sim-ctl.log \
  | grep -o 'assignment::tests::[a-z_0-9]*' | sort -u || true)
if [ "$control_failures" != "assignment::tests::a_mistyped_key_is_named_rather_than_silently_dropped" ]; then
  echo "  FAIL: the control failed on something other than the suggestion:" >&2
  echo "$control_failures" >&2
  tail -30 /tmp/w8-sim-ctl.log >&2
  exit 1
fi
echo "  OK: the control fails on the mistyped-key assertion, and only there."

# The file is restored byte for byte, and saying so is better than assuming it.
if grep -q 'NEGATIVE CONTROL' modules/crm-intake/src/assignment.rs; then
  echo "  FAIL: the control is still in the source tree." >&2
  exit 1
fi
echo "  OK: the source is restored."
