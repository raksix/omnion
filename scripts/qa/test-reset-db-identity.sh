#!/usr/bin/env bash
# Regression test for reset-db.sh's database identity.
#
# The hazard this exists for is destructive and silent: reset-db.sh DROPs a database, and it used to
# default to `omnion_qa` — the MAIN writer's — whatever stack asked for it. A pass run through
# run.sh was never wrong (run.sh exports QA_DB), so nothing in the harness could catch it; only a
# direct call could, and a direct call is exactly what a human does while debugging a pass. On
# 2026-10-02 tick 68 this writer ran it that way against a private container and the transcript
# read "database omnion_qa does not exist, skipping" — which was luck, not design: on the shared
# container that name is very much alive.
#
# The test proves three things separately, because they are three different claims:
#   1. a non-main stack resolves to its OWN database (the fix works),
#   2. an unknown/typo'd name is REFUSED rather than dropped,
#   3. `docker exec` is never reached in either refusal case (the guard is before the destructive
#      call, not after it — a guard that prints a warning and then drops is not a guard).
#
# It runs the REAL script with a fake `docker` on PATH that records its arguments instead of
# executing them, so nothing is dropped and the assertion is on the command the script WOULD run.
set -uo pipefail

SCRIPT="$(dirname "${BASH_SOURCE[0]}")/reset-db.sh"
FAKE_BIN="$(mktemp -d)"
LOG="$FAKE_BIN/docker.log"
trap 'rm -rf "$FAKE_BIN"' EXIT

# A `docker` that records and succeeds. The real one is never invoked.
cat > "$FAKE_BIN/docker" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$DOCKER_LOG"
exit 0
EOF
chmod +x "$FAKE_BIN/docker"
export DOCKER_LOG="$LOG"
export PATH="$FAKE_BIN:$PATH"

fails=0
check() { # check <label> <expected> <actual>
  if [ "$2" = "$3" ]; then
    echo "  ok   — $1"
  else
    echo "  FAIL — $1 (expected: $2, got: $3)"
    fails=$((fails + 1))
  fi
}

# `reset-db.sh` deliberately lets an explicit QA_DB win over the derived name, because run.sh
# exports it. That makes these derivation checks measure whatever QA_DB happens to be in the
# ambient environment instead of the derivation itself: run from a pass, QA_STACK=w2 inherits
# QA_DB=omnion_qa_w2, so the "w2 derives its own database" checks pass *by coincidence* and
# "QA_STACK=main keeps omnion_qa" — which wants the derivation to produce a different name —
# cannot pass at all. A test that reads the environment instead of the code is not a test.
# `env -u QA_DB` is what makes the case say what it means.
: > "$LOG"
out="$(env -u QA_DB QA_STACK=w2 bash "$SCRIPT" 2>&1)"
got_db="$(grep -oE 'CREATE DATABASE [a-z0-9_]+' "$LOG" | head -1 | awk '{print $3}')"
check "QA_STACK=w2 drops omnion_qa_w2, not omnion_qa" "omnion_qa_w2" "$got_db"
check "QA_STACK=w2 never names the main writer's database" "" \
  "$(grep -oE 'DROP DATABASE IF EXISTS omnion_qa WITH' "$LOG" | head -1)"

# the same, on the stack this branch actually runs
: > "$LOG"
env -u QA_DB QA_STACK=w2 bash "$SCRIPT" >/dev/null 2>&1
got_db="$(grep -oE 'CREATE DATABASE [a-z0-9_]+' "$LOG" | head -1 | awk '{print $3}')"
check "the stack this branch runs resolves to its own database" "omnion_qa_w2" "$got_db"

# --- 2. main keeps the shared name --------------------------------------------------------
: > "$LOG"
env -u QA_DB QA_STACK=main bash "$SCRIPT" >/dev/null 2>&1
got_db="$(grep -oE 'CREATE DATABASE [a-z0-9_]+' "$LOG" | head -1 | awk '{print $3}')"
check "QA_STACK=main keeps omnion_qa" "omnion_qa" "$got_db"

# ...and it can fail: derivation that ignores QA_STACK would hand the main writer a private
# database named after whoever happened to ask last. Removing the derivation from reset-db.sh
# makes this check red.
sed 's/^\[ "\$STACK" != "main" \] && DEFAULT_DB="omnion_qa_\${STACK}"/:/' "$SCRIPT" \
  > "$SCRIPT.derived-less"
chmod +x "$SCRIPT.derived-less"
: > "$LOG"
env -u QA_DB QA_STACK=w2 QA_DERIVED_LESS=1 bash "$SCRIPT.derived-less" >/dev/null 2>&1
got_db="$(grep -oE 'CREATE DATABASE [a-z0-9_]+' "$LOG" | head -1 | awk '{print $3}')"
check "and it can fail: without the derivation every stack drops the main writer's database" \
  "omnion_qa" "$got_db"
rm -f "$SCRIPT.derived-less"

# an explicit override still wins — run.sh exports QA_DB, and a pass must not be second-guessed
: > "$LOG"
QA_STACK=w2 QA_DB=omnion_qa_explicit bash "$SCRIPT" >/dev/null 2>&1
got_db="$(grep -oE 'CREATE DATABASE [a-z0-9_]+' "$LOG" | head -1 | awk '{print $3}')"
check "an explicit QA_DB still wins over the derived name" "omnion_qa_explicit" "$got_db"

# --- 3. a name that is not a QA database is REFUSED, before docker is reached --------------
: > "$LOG"
out="$(QA_STACK=w2 QA_DB=development bash "$SCRIPT" 2>&1)"; rc=$?
check "a non-QA database name exits non-zero" "2" "$rc"
check "and never reaches docker" "0" "$(wc -l < "$LOG" | tr -d ' ')"

: > "$LOG"
out="$(QA_STACK=w2 QA_DB=omnion bash "$SCRIPT" 2>&1)"; rc=$?
check "the development database is refused" "2" "$rc"
check "and never reaches docker" "0" "$(wc -l < "$LOG" | tr -d ' ')"

# a typo that merely LOOKS like a QA name: a trailing character is a different database
: > "$LOG"
out="$(QA_STACK=w2 QA_DB=omnion_qa_ bash "$SCRIPT" 2>&1)"; rc=$?
check "a malformed QA name is refused rather than guessed at" "2" "$rc"
check "and never reaches docker" "0" "$(wc -l < "$LOG" | tr -d ' ')"

# The refusal must SAY which name it refused, or an operator cannot act on it.
# Run it in a subshell and read the message directly: `... | grep -q` makes the check's own exit
# status the grep's, so a PASS here would have been the grep's opinion and not the script's.
refusal="$(QA_STACK=w2 QA_DB=development bash "$SCRIPT" 2>&1)"
if printf '%s' "$refusal" | grep -q "development"; then
  echo "  ok   — the refusal names the database it refused"
else
  echo "  FAIL — the refusal does not name the database (got: $refusal)"
  fails=$((fails + 1))
fi

echo
if [ "$fails" -eq 0 ]; then
  echo "PASS — reset-db.sh derives the database from the stack and refuses anything else"
  exit 0
fi
echo "FAIL — $fails check(s) failed"
exit 1