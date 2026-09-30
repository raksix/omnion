#!/usr/bin/env bash
# Omnion QA — a pass that never started must be able to say so.
#
# The harness had a way to report a verdict and no way to report *nothing happened*.
# `run.sh` created its artifact directory on line 17, took a QA slot on line 64 and did
# not install a trap until line 92. A pass killed while queued therefore left an EMPTY
# `qa-artifacts/<ts>/` directory: the directory says "a pass started here", and the next
# tick's `ls -1t qa-artifacts` counts it as a pass that ran and produced nothing.
#
# That is not a w6 problem. On 2026-09-30 five of nine worktrees had empty artifact
# directories, two of them w6's, and the passes that left them printed exactly one line:
# "waiting for a QA slot".
#
# The fix: the directory is created before the wait as an explicit QUEUED record with a
# `summary.json` that declares itself void, and the record is retired once the pass owns a
# slot. This gate RUNS `run.sh` under the real failure — the slot already taken, so the
# pass blocks and never reaches the walkthrough — and reads what the pass left behind. It
# does not grep `run.sh` for the shape of the fix, because a regex passes against prose
# that documents a bug as readily as against the code that fixes it.
#
# The first version of this gate ran the real `run.sh` too, and was wrong: it could not
# redirect where `run.sh` put its artifacts, so it reset a real QA database and wrote into
# the repository's own `qa-artifacts/`. The first red it produced was its own. `run.sh`
# derives ROOT from its own location, so the only honest way to redirect it is to run a
# COPY of the script placed in a scratch tree.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

pass=0
fail=0
check() { # description, expected, actual
  if [ "$2" = "$3" ]; then
    printf 'ok   %s\n' "$1"
    pass=$((pass + 1))
  else
    printf 'FAIL %s\n       expected: %s\n       actual:   %s\n' "$1" "$2" "$3"
    fail=$((fail + 1))
  fi
}

tmp="$(mktemp -d)"
holder_pid=""
cleanup() { [ -n "$holder_pid" ] && kill "$holder_pid" 2>/dev/null; rm -rf "$tmp"; }
trap cleanup EXIT

# Stage a copy of run.sh in a scratch repository, so ROOT (and therefore qa-artifacts/,
# apps/ and the cargo target) resolve inside $tmp and nothing here can touch the real tree
# or a real database. `scripts/qa/run.sh` is the only file the pass needs before it blocks.
stage_runsh() { # <label> -> echoes the path to the staged copy
  local label="$1"
  local dir="$tmp/$label"
  mkdir -p "$dir/scripts/qa" "$dir/apps/admin/node_modules/next/dist/bin" "$dir/apps/web/node_modules/next/dist/bin"
  cp "$ROOT/scripts/qa/run.sh" "$ROOT/scripts/qa/qa-slot.sh" "$dir/scripts/qa/"
  # The pass blocks on the slot and dies before these, but `set -e` plus a missing binary
  # would otherwise make the outcome depend on which step it reached.
  printf '#!/bin/sh\nexit 0\n' > "$dir/scripts/qa/reset-db.sh"
  printf '#!/bin/sh\nexit 0\n' > "$dir/scripts/qa/cargo-slot.sh"
  chmod +x "$dir/scripts/qa"/*.sh
  printf '%s' "$dir/scripts/qa/run.sh"
}

# Run a staged pass with the slot already held by a live process, so it blocks in the wait
# and is killed at the timeout. The holder stays alive for the whole call, so the reaper
# cannot decide the place is stale and hand it to the waiter.
run_pass_blocked_on_slot() { # <label> <staged run.sh>
  local label="$1"
  local runsh="$2"
  local slotdir="$tmp/$label-slot"
  mkdir -p "$slotdir" "$slotdir-holders"
  sleep 60 &
  holder_pid=$!
  printf '%s' "$holder_pid" > "$slotdir-holders/1-1"
  : > "$slotdir/1-1"
  QA_SLOT_DIR="$slotdir" \
  QA_SLOTS=1 \
  QA_SLOT_WAIT=600 \
  QA_SLOT_REAP_GRACE=999999 \
  QA_STACK=gate \
  QA_API_PORT=18099 \
  QA_ADMIN_PORT=31099 \
  QA_WEB_PORT=32099 \
    timeout -s KILL 20 bash "$runsh" > "$tmp/$label.log" 2>&1
  holder_pid=""
}

# A copy of today's run.sh with ONLY the fix reverted: the queued record, the pre-wait trap
# and the stamp that retires it.
stage_prefix_runsh() { # -> echoes the path
  local dir="$tmp/prefix"
  mkdir -p "$dir/scripts/qa"
  python3 - "$ROOT/scripts/qa/run.sh" "$dir/scripts/qa/run.sh" <<'PY'
import re, sys
src = open(sys.argv[1]).read()
# Drop the queued-record block and the pre-wait trap, and put `mkdir -p "$OUT"` back on
# the first line where it was — which is the defect, reproduced rather than described.
src = re.sub(r'\nmkdir -p "\$OUT"\nwrite_queued_record\(\) \{.*?\ntrap queued_exit EXIT INT TERM\n', '\n', src, flags=re.S)
src = re.sub(r'\n# The pass has a place and is about to do real work.*?\nrm -f "\$OUT/QUEUED\.md"\n', '\n', src, flags=re.S)
src = src.replace('TS="$(date -u +%Y%m%d-%H%M%S)"\nOUT="$ROOT/qa-artifacts/$TS"',
                  'TS="$(date -u +%Y%m%d-%H%M%S)"\nOUT="$ROOT/qa-artifacts/$TS"\nmkdir -p "$OUT"')
open(sys.argv[2], 'w').write(src)
PY
  printf '%s' "$dir/scripts/qa/run.sh"
}

artifacts_for() { # <staged run.sh path> -> the artifact directory the pass created
  local dir root
  dir="$(dirname "$(dirname "$(dirname "$1")")")"
  find "$dir/qa-artifacts" -maxdepth 1 -type d -name '20??????-??????' 2>/dev/null | sort | tail -1
}

# 1. A pass killed while queued must leave a record that says it never ran.
after_runsh="$(stage_runsh after)"
run_pass_blocked_on_slot after "$after_runsh"
after_art="$(artifacts_for "$after_runsh")"

if [ -n "$after_art" ] && [ -f "$after_art/QUEUED.md" ] && [ -s "$after_art/summary.json" ]; then
  check "after: a pass blocked on the slot writes a queued record" "present" "present"
else
  check "after: a pass blocked on the slot writes a queued record" "present" "absent"
fi

if [ -n "$after_art" ] && [ -f "$after_art/QUEUED.md" ] \
   && grep -q 'QA slot' "$after_art/QUEUED.md"; then
  printf 'ok   after: the record names the queue as the cause\n'; pass=$((pass + 1))
else
  printf 'FAIL after: the record does not name the queue\n'; fail=$((fail + 1))
fi

# 2. The record must be able to say "void", or it can be read as a clean pass.
verdict="$(python3 - "$after_art/summary.json" <<'PY' 2>/dev/null || echo missing
import json, sys
s = json.load(open(sys.argv[1]))
# The walkthrough writes a summary without this key; only the queued record sets it.
print("True" if s.get("void") is True else s.get("void", "missing"))
PY
)"
check "after: the summary marks itself void" "True" "$verdict"

# 3. The gate has to discriminate, or it is a decoration: the pre-fix script must FAIL
#    these same checks.
prefix_runsh="$(stage_prefix_runsh)"
run_pass_blocked_on_slot before "$prefix_runsh"
before_art="$(artifacts_for "$prefix_runsh")"

if [ -n "$before_art" ] && [ -f "$before_art/QUEUED.md" ]; then
  check "before: a pass blocked on the slot records nothing" "absent" "present"
else
  check "before: a pass blocked on the slot records nothing" "absent" "absent"
fi

if [ -n "$before_art" ] && [ -s "$before_art/summary.json" ]; then
  printf 'FAIL before: the pre-fix pass left a summary, so this is not the pre-fix script\n'
  fail=$((fail + 1))
else
  printf 'ok   before: the pre-fix pass leaves the empty directory, not a record\n'
  pass=$((pass + 1))
fi

# 4. Nothing escaped the scratch tree. The first version of this gate reset a real QA
#    database and wrote into the repository's qa-artifacts/ because it could not redirect
#    where run.sh put them.
new_in_repo="$(find "$ROOT/qa-artifacts" -maxdepth 1 -type d -name '20??????-??????' -newermt '-3 minutes' 2>/dev/null | wc -l)"
check "the gate wrote nothing into the repository's qa-artifacts" "0" "$new_in_repo"
gate_dbs="$(docker exec "${QA_PG_CONTAINER:-omnion-postgres}" psql -U omnion -d postgres -t -A \
  -c "select count(*) from pg_database where datname like 'omnion_qa_gate%'" 2>/dev/null || echo 0)"
check "the gate created no QA database" "0" "${gate_dbs:-0}"

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
