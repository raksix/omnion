#!/usr/bin/env bash
# Omnion QA — the `arg()` contract in walkthrough.cjs.
#
# `arg()` is the only thing that reads the pass's command line, and a flag it does not
# understand is invisible: the caller gets the fallback, the scope silently widens to
# everything, and every artifact the pass writes claims the narrower coverage it never ran.
# That is the failure this file exists to make impossible to reintroduce.
#
#   run.sh builds the scope as `--only="$QA_ONLY"`. The paired form `--only VALUE` is also
#   honoured, because a hand-run pass uses it. Both must work, and both must stay distinct:
#   a scope that parses under one form and not the other is a scope that is sometimes real.
#
#   bash scripts/qa/arg-test.sh
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

pass=0
fail=0

# Extract the `arg` function from the real source rather than restating it. A test that
# carries its own copy of the logic passes against a copy that agrees with it, and the
# defect this file was written for is a defect in the copy's owner.
sed -n '/^function arg(name, fallback) {/,/^}/p' \
  "$ROOT/scripts/qa/walkthrough.cjs" > "$WORK/arg.js"
if [ ! -s "$WORK/arg.js" ]; then
  echo "FAIL  could not extract arg() from scripts/qa/walkthrough.cjs"
  echo "arg-test: 0/$((pass + fail + 1))"
  exit 1
fi

check() { # description, expected, argv...
  local description="$1" expected="$2"
  shift 2
  local actual
  actual="$(printf '%s\n' "$@" > "$WORK/argv.txt"
    node -e '
      const fs = require("fs");
      const src = fs.readFileSync(process.argv[1], "utf8");
      const argv = fs.readFileSync(process.argv[2], "utf8").split("\n").filter(Boolean);
      // Mirror the real module scope: walkthrough.cjs calls `arg(...)` at the top level.
      const run = new Function("process", "argv", `${src}\nreturn arg("only", "all");`)
        .call(null, { argv }, argv);
      process.stdout.write(String(run));
    ' "$WORK/arg.js" "$WORK/argv.txt")"
  if [ "$actual" = "$expected" ]; then
    pass=$((pass + 1))
    echo "ok    $description"
  else
    fail=$((fail + 1))
    echo "FAIL  $description — expected '$expected', got '$actual'"
  fi
}

# 1. The form run.sh actually sends. This is the case that was broken: `--only=a,b` parsed as
#    nothing, returned "all", and the pass walked every route in the product.
check "the inline form run.sh sends" \
  "organizations,organizationDepth" \
  "--only=organizations,organizationDepth"

# 2. The paired form a hand-run pass sends, and the one the parser was written for.
check "the paired form" \
  "organizations" \
  "--only" "organizations"

# 3. Absent means the fallback — the full pass, on purpose.
check "absent falls back to the full pass" \
  "all" \
  "--url" "http://127.0.0.1:3100"

# 4. A flag with an empty value is a deliberate empty scope, not a missing one. Returning the
#    fallback here would widen a pass someone tried to scope to nothing.
check "an empty value stays empty" \
  "" \
  "--only="

# 5. Prefix safety: `--only` must not be read out of `--only-extra`, or a future flag would
#    silently steal the scope.
check "a longer flag name is not read as this one" \
  "all" \
  "--only-extra=narrow" "--url" "x"

# 6. A value that looks like another flag is still a value.
check "a value that looks like a flag" \
  "--all" \
  "--only" "--all"

echo
echo "arg-test: $pass/$((pass + fail))"
[ "$fail" -eq 0 ]
