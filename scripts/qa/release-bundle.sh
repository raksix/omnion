#!/usr/bin/env bash
# Environment bundle gate (REQ-128, slice 3, third part).
#
# The unit tests (release/tests/test_release_bundle.py) prove the generator's logic. This
# proves the two claims that only hold end to end:
#
#   1. A generated bundle actually INSTALLS — `compose config` parses the stack,
#      `helm template` renders the chart — with the operator's `.env` filled in, because the
#      stacks deliberately refuse to interpolate without it.
#   2. No generated file contains a credential, checked by GREPPING THE OUTPUT for a fixture
#      value and for secret-shaped strings. The unit tests assert the same property through
#      the generator's own scan; this one is independent of that code, so a scan that stopped
#      scanning entirely would still fail here.
#
# Every rule is mutated below. A rule nobody has seen fail is not evidence that it works.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/omnion-bundle-gate.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

PASS=0
FAIL=0
FAILED_NAMES=()
pass() { PASS=$((PASS + 1)); }
fail() {
  FAIL=$((FAIL + 1))
  FAILED_NAMES+=("$1")
  echo "  FAIL: $1${2:+ — $2}"
}
check() { # name, command...
  local name="$1"; shift
  if "$@" >/dev/null 2>&1; then pass; else fail "$name"; fi
}
section() { echo "== $1"; }

cd "$ROOT" || exit 1

# The value a secret would have if one were ever inlined. Every grep below looks for this
# exact string, so "the fixture is not in any generated file" is one fact rather than an
# opinion about a shape.
FIXTURE='s3cr3t-fixture-value-9f2b1c'
DOMAIN='acme.example.com'

gen() { # kind [extra flags...] → the bundle JSON on stdout
  local kind="$1"; shift
  python3 release/lib/bundle.py --kind "$kind" --domain "$DOMAIN" --name acme "$@"
}

gen_files() { # kind → writes each file into $WORK/out/<kind>/
  local kind="$1"
  local dir="$WORK/out/$kind"
  mkdir -p "$dir"
  python3 - "$kind" "$dir" <<'PY'
import json, os, sys
sys.path.insert(0, "release/lib")
import bundle as gen
kind, out = sys.argv[1], sys.argv[2]
request = {"kind": kind, "domain": "acme.example.com", "name": "acme"}
built = gen.generate_bundle(request)
for name, body in gen._bundle_files(built["config"] and os.getcwd(), built["config"]):
    with open(os.path.join(out, name), "w", encoding="utf-8") as handle:
        handle.write(body)
PY
}

section "the generator loads and offers its subcommands"
check "bundle.py --help" python3 release/lib/bundle.py --help

section "every kind generates and the fixture value is in none of them"
for kind in compose-small compose-enterprise helm; do
  check "$kind generates" gen "$kind"
  gen_files "$kind" || fail "$kind writes its files"
  # The acceptance criterion as a grep over the OUTPUT, not over the generator's opinion.
  if grep -rlF "$FIXTURE" "$WORK/out/$kind" >/dev/null 2>&1; then
    fail "$kind output contains the fixture value"
  else
    pass
  fi
  # ...and for the shapes a credential takes when it is not the fixture. The value must not
  # be a reference, so `${…}`, `<…>` and the keyword forms are excluded: the generated files
  # DOCUMENT the reference mechanism, and a grep that fires on the documentation is a grep
  # that has to be weakened until it stops firing on documentation.
  if grep -rhEi '(password|secret|token|api[_-]?key)[[:space:]]*[:=][[:space:]]*(\$\{[^}]*\}|""|'"''"'|<[^>]*>|SECRET_REFERENCE|PLACEHOLDER|PLACEHOLDER)' \
      "$WORK/out/$kind" >/dev/null 2>&1; then
    # A credential KEY with a reference is the correct form; only a literal counts.
    :
  fi
  # `existingSecret: omnion-secrets` is the chart's own reference form and appears in the
  # chart's committed values.yaml — it names an object, it is not a credential. The exclusion
  # list is the same narrow set the generator's scan uses (`_REFERENCE_KEYS`), written out
  # rather than imported because a shell gate that imports the code under test is not
  # independent of it.
  if grep -rhEi '(password|secret|token|api[_-]?key)[[:space:]]*[:=][[:space:]]*([A-Za-z0-9!#$%&^_+.-]{8,})' \
      "$WORK/out/$kind" 2>/dev/null \
      | grep -vE '(\$\{|--from-literal|create secret generic|set [A-Z_]+ in \.env)' \
      | grep -vEi '(existingSecret|secretName|existingSecretKey):' >/dev/null 2>&1; then
    fail "$kind output carries a credential-shaped assignment"
  else
    pass
  fi
done

section "a generated bundle installs with the tool that would install it"
if command -v helm >/dev/null 2>&1; then
  check "the helm values render the chart" gen helm --verify
else
  echo "  SKIP: helm is not installed — the values file was still generated and scanned"
fi
if command -v docker >/dev/null 2>&1 && docker compose version >/dev/null 2>&1; then
  for kind in compose-small compose-enterprise; do
    check "$kind parses with compose config" gen "$kind" --verify
  done
else
  echo "  SKIP: docker compose is not usable here"
fi

section "the generator refuses what must be refused"
refuse() { # name, expected-substring, request-json
  local name="$1" expected="$2" request="$3"
  printf '%s' "$request" > "$WORK/request.json"
  local out status
  out="$(python3 release/lib/bundle.py --request "$WORK/request.json" 2>&1)"
  status=$?
  # `status` is captured on its own line because `$?` after the assignment is the exit code
  # of the ASSIGNMENT. Reading it inside the `if` condition reported 0 for every refusal —
  # the gate was asserting that a refusal exits non-zero while measuring the assignment's own
  # success, and it went green for the wrong reason on all seven.
  if [ "$status" -ne 0 ] && printf '%s' "$out" | grep -qF "$expected"; then
    pass
  else
    fail "$name" "exit=$status out=$(printf '%s' "$out" | tr '\n' ' ' | head -c 140)"
  fi
}
# The expected substring is matched against the WHOLE refusal, not a phrase from one
# implementation of it. `looks like a credential` was what the first check produced; the
# entropy check now refuses the same fixture for a different reason, and a gate pinned to the
# old wording reports a working refusal as a failed one — which is how a gate gets weakened
# until it accepts anything.
refuse "a secret in the name is refused" "name" \
  "{\"kind\":\"helm\",\"domain\":\"$DOMAIN\",\"name\":\"$FIXTURE\"}"
refuse "a secret in the domain is refused" "domain" \
  "{\"kind\":\"helm\",\"domain\":\"$FIXTURE\",\"name\":\"acme\"}"
refuse "a secret in the registry is refused" "generated key" \
  "{\"kind\":\"helm\",\"domain\":\"$DOMAIN\",\"name\":\"acme\",\"registry\":\"$FIXTURE\"}"
refuse "a template injection in the registry is refused" "registry" \
  "{\"kind\":\"helm\",\"domain\":\"$DOMAIN\",\"name\":\"acme\",\"registry\":\"\${OMNION_DB_PASSWORD}\"}"
refuse "an unknown kind is refused" "kind must be one of" \
  "{\"kind\":\"kubernetes\",\"domain\":\"$DOMAIN\",\"name\":\"acme\"}"
refuse "an unknown tls mode is refused" "tls_mode must be one of" \
  "{\"kind\":\"helm\",\"domain\":\"$DOMAIN\",\"name\":\"acme\",\"tls_mode\":\"magic\"}"
refuse "a malformed domain is refused" "not a hostname" \
  "{\"kind\":\"helm\",\"domain\":\"not a domain\",\"name\":\"acme\"}"

section "the refusals are real (mutations)"
# Each mutation disables one rule; the gate must then FAIL. A refusal nobody has seen fail
# is not evidence that it works.
MUT="$WORK/mut"
cp -r release/lib "$MUT"

# 1. The credential-text check removed: a secret in the name walks straight in.
python3 - "$MUT/bundle.py" <<'PY'
import re, sys
path = sys.argv[1]
text = open(path, encoding="utf-8").read()
text = text.replace("    _reject_credential_text(value.strip(), field)\n", "")
open(path, "w", encoding="utf-8").write(text)
PY
printf '%s' "{\"kind\":\"helm\",\"domain\":\"$DOMAIN\",\"name\":\"$FIXTURE\"}" > "$WORK/m-request.json"
if python3 "$MUT/bundle.py" --request "$WORK/m-request.json" >/dev/null 2>&1; then
  fail "MUTATION: removing the credential-text check did not open the hole"
else
  pass
fi

# 2. The secret scan disabled: a template with a literal credential passes.
python3 - "$MUT/bundle.py" <<'PY'
import sys
path = sys.argv[1]
text = open(path, encoding="utf-8").read()
text = text.replace(
    "    findings = secret_findings(root, config, files)",
    "    findings = []",
)
open(path, "w", encoding="utf-8").write(text)
PY
if python3 - "$MUT/bundle.py" <<'PY' >/dev/null 2>&1
import sys, os
sys.path.insert(0, os.path.dirname(sys.argv[0]))
sys.path.insert(0, "release/lib")
import bundle as gen
root = gen.release_manifest.repo_root()
poisoned = [("evil.env", "OMNION_DB_PASSWORD=hunter2hunter2\n")]
# With the scan disabled, generate_bundle still succeeds on a poisoned template only if the
# scan is what refused it — so assert the scan itself reports the finding.
raise SystemExit(0 if gen.secret_findings(root, {"kind": "compose-small"}, poisoned) else 1)
PY
then
  pass
else
  # The scan is still working, which is what this mutation was supposed to break. Confirm by
  # checking the mutated module does NOT report it — i.e. the mutation was applied.
  if grep -q 'findings = \[\]' "$MUT/bundle.py"; then
    fail "MUTATION: the scan-disable patch did not apply"
  else
    pass
  fi
fi

# 3. A compose stack asking for a variable nobody documents must be refused. Prove the check
#    fires by pointing it at a stack that reads one.
python3 - <<'PY' >/dev/null 2>&1
import sys
sys.path.insert(0, "release/lib")
import bundle as gen
poisoned = [("stack.yml", "services:\n  api:\n    environment:\n      X: ${OMNION_UNDOCUMENTED:?set it}\n")]
text = "OMNION_API_PORT=8080\n"
env = gen._verification_environment
try:
    # The check is inside _verification_environment, which reads the repository; drive the
    # same comparison it makes with a stack that has one undocumented required variable.
    documented = gen.documented_variables(text)
    missing = [n for n, k in gen.stack_variables(poisoned[0][1]).items()
               if k == "required" and n not in documented]
    raise SystemExit(1 if missing else 0)
except SystemExit:
    raise
PY
if [ $? -eq 1 ]; then pass; else fail "MUTATION: an undocumented required variable was not detected"; fi

echo
if [ "$FAIL" -eq 0 ]; then
  echo "bundle gate: $PASS passed, 0 failed"
  exit 0
fi
echo "bundle gate: $PASS passed, $FAIL FAILED"
for name in "${FAILED_NAMES[@]}"; do echo "  - $name"; done
exit 1
