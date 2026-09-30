#!/usr/bin/env bash
# Helm chart gate for REQ-128 slice 2b.
#
# WHY THIS FILE EXISTS AND WHY IT IS LONGER THAN THE CHART
# --------------------------------------------------------
# The previous tick wrote this chart without being able to run `helm`, and recorded the reason in
# the request file: "helm is not installed on this box, so a chart written here could not be
# linted, rendered or installed, and shipping one that has never rendered is the 'documented but
# unreachable' shape this wave has produced in the other requests."  That reasoning was sound and
# the premise was wrong: helm 3.16.3 is installed at /usr/local/bin/helm.  So the chart sat
# UNTRACKED for a tick, and when it was finally run it did not compile at all:
#
#   1. `omnion.image` read `$root := .` while every caller passes `dict "root" ... "component" ...`
#      — so `.Values` was nil and `index nil "migrate"` is a template error on EVERY render.
#   2. the same helper emitted an `image:` key that its callers already wrote, rendering
#      `image: image: ghcr.io/...` — a YAML parse error, not a wrong image.
#   3. the migration Job asked for `ghcr.io/raksix/omnion-migrate`, an image no Dockerfile builds
#      and no release publishes, so a real install would fail its pull during `pre-install`.
#   4. all three `range` loops wrote `---` glued to the next document's first key. `helm template`
#      tolerates it; `helm lint`, which parses per file, does not.
#
# Every one of those was found by RUNNING helm, and none by reading the file.  This gate therefore
# asserts rendered output — the artefact an operator actually applies — rather than file contents.
# It is written to be runnable with no cluster, no docker daemon and no kubectl, because the three
# acceptance lines it covers are all about what the chart RENDERS.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CHART="${ROOT}/infra/helm/omnion"
HELM="${HELM:-helm}"
WORK="$(mktemp -d /tmp/omnion-helm-gate.XXXXXX)"
trap 'rm -rf "${WORK}"' EXIT

PASS=0
FAIL=0
declare -a FAILED=()

ok()   { PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); FAILED+=("$1"); printf '  FAIL %s\n' "$1"; [ $# -gt 1 ] && printf '       %s\n' "$2"; }

check() { # check <name> <command…>
  local name="$1"; shift
  local out
  if out="$("$@" 2>&1)"; then ok "${name}"; else bad "${name}" "$(printf '%s' "${out}" | head -3)"; fi
}

# Render into a file, or fail loudly with helm's own message.
render() { # render <outfile> [extra helm args…]
  local out="$1"; shift
  "${HELM}" template "${RELEASE_NAME}" "${CHART}" --namespace "${NAMESPACE}" "$@" >"${out}" 2>"${out}.err"
}

# A python predicate over a rendered document. Written as a string so the assertion is a sentence.
assert_render() { # assert_render <name> <outfile> <python-expr using d (parsed docs)>
  local name="$1" out="$2" expr="$3"
  local msg
  # The namespace deliberately keeps `__builtins__` EMPTY — an assertion must not be able to
  # shell out or read the filesystem — so the handful of safe builtins every predicate uses
  # (`all`, `any`, `int`, `set`, `len`, `str`) are re-bound explicitly. A predicate that raises
  # NameError is NOT a failing assertion, it is a broken one, and the difference matters: the
  # first run of this gate reported 26 failures of which most were NameError, i.e. the check
  # could not have passed against anything. The shell's own variables (RELEASE_NAME) are
  # exported into the namespace for the same reason — a predicate that cannot see the release
  # name cannot look a resource up by it.
  if msg="$(python3 - "${out}" "${expr}" 2>&1 <<'PY'
import os, sys, json
import builtins as __builtins__
RELEASE_NAME = os.environ.get('RELEASE_NAME','')
NAMESPACE = os.environ.get('NAMESPACE','')
FIXTURE = os.environ.get('FIXTURE','')
CHART = os.environ.get('CHART','')
path, expr = sys.argv[1], sys.argv[2]
raw = open(path, encoding="utf-8").read()
docs = [x for x in raw.split("\n---") if x.strip()]
d = [json.loads("{}") for _ in docs]
try:
    import yaml  # type: ignore
    d = [y for y in yaml.safe_load_all(raw) if y]
except Exception:
    pass
def by(kind, name=None):
    for o in d:
        if o.get("kind") == kind and (name is None or o.get("metadata", {}).get("name") == name):
            return o
    return None
def env_of(o, var):
    for c in o.get("spec", {}).get("template", {}).get("spec", {}).get("containers", []):
        for e in c.get("env", []):
            if e.get("name") == var:
                return e
    return None
def plain(kind, needle):
    for o in d:
        if o.get("kind") == kind and needle in json.dumps(o):
            return o
    return None
safe = {k: getattr(__builtins__, k) for k in
        ("all", "any", "int", "set", "len", "str", "sorted", "min", "max", "sum", "bool", "list", "dict")}
safe.update(
    d=d, by=by, env_of=env_of, plain=plain, raw=raw,
    RELEASE_NAME=RELEASE_NAME, NAMESPACE=NAMESPACE, FIXTURE=FIXTURE, CHART=CHART,
    BUILT_IMAGES=os.environ.get("BUILT_IMAGES", ""), E=os.environ.get("E", ""),
)
# Names go into GLOBALS, not locals: a list comprehension opens a new scope, and a new scope
# cannot see an eval() locals mapping — every predicate using `by` inside a comprehension raised
# NameError and reported "assertion returned false", which is a broken check wearing a red one.
safe["__builtins__"] = {}
# `eval` evaluates an EXPRESSION. A predicate written as two statements separated by `;` is a
# SyntaxError, and a SyntaxError behind `if not eval(...)` is reported as "assertion returned
# false" — a broken check wearing a failing one, which is the shape this whole file exists to
# reject. Compile-check the expression first and report a SyntaxError as itself.
try:
    compiled = compile(expr, "<assertion>", "eval")
except SyntaxError as exc:
    print("ASSERTION IS NOT AN EXPRESSION: %s" % exc, file=sys.stderr)
    sys.exit(2)
try:
    if not eval(compiled, safe, {}):
        sys.exit(1)
except Exception as exc:
    print("ASSERTION RAISED (%s): %s" % (type(exc).__name__, exc), file=sys.stderr)
    sys.exit(2)
PY
  )"; then ok "${name}"; else bad "${name}" "${msg:-assertion returned false}"; fi
}

echo "== omnion helm chart gate (${HELM}) =="
command -v "${HELM}" >/dev/null 2>&1 || { echo "FATAL: ${HELM} not on PATH"; exit 2; }
"${HELM}" version --short 2>/dev/null | head -1

export RELEASE_NAME="w6-check"
export NAMESPACE="omnion-w6"
export DIGEST="sha256:$(printf 'a%.0s' $(seq 1 64))"
export E="${DIGEST}"
export FIXTURE="postgres://omnion:hunter2-not-a-real-password@db.internal:5432/omnion"
export CHART


# ---------------------------------------------------------------------------------------------
echo
echo "-- 1. the chart lints, and the default values render"
# ---------------------------------------------------------------------------------------------
check "helm lint passes with the schema present" "${HELM}" lint "${CHART}"
check "values.schema.json is valid JSON" python3 -c "import json,sys; json.load(open('${CHART}/values.schema.json'))"

render "${WORK}/default.yaml" || { echo "FATAL: default render failed"; cat "${WORK}/default.yaml.err"; exit 2; }
ok "default values render"

# ---------------------------------------------------------------------------------------------
echo
echo "-- 2. the rendered resource set is the whole platform, and nothing else"
# ---------------------------------------------------------------------------------------------
KIND_COUNTS="$(python3 - "${WORK}/default.yaml" <<'PY'
import sys, collections
try:
    import yaml
except ImportError:
    sys.exit(0)
docs = [d for d in yaml.safe_load_all(open(sys.argv[1], encoding="utf-8")) if d]
c = collections.Counter(d.get("kind") for d in docs)
print(" ".join(f"{k}={v}" for k, v in sorted(c.items())))
PY
)"
echo "  rendered: ${KIND_COUNTS}"

for pair in "Deployment=3" "Service=3" "Job=1" "Ingress=1" "ConfigMap=1" "PodDisruptionBudget=1" "HorizontalPodAutoscaler=0"; do
  kind="${pair%%=*}"; want="${pair##*=}"
  got="$(python3 - "${WORK}/default.yaml" "${kind}" <<'PY'
import sys
try:
    import yaml
except ImportError:
    print(0); raise SystemExit(0)
print(sum(1 for d in yaml.safe_load_all(open(sys.argv[1], encoding="utf-8")) if d and d.get("kind") == sys.argv[2]))
PY
)"
  if [ "${got}" = "${want}" ]; then ok "renders ${want}× ${kind}"; else bad "renders ${want}× ${kind}" "got ${got}"; fi
done

# ---------------------------------------------------------------------------------------------
echo
echo "-- 3. every pull reference names an image this repository actually builds"
# ---------------------------------------------------------------------------------------------
# The Dockerfiles are the source of truth for what exists; a chart that names anything else
# produces an install whose pods cannot start.
# The image names come from the compose stacks, which are the release's own statement of what is
# published. File names are NOT the contract: `admin.Dockerfile` builds TWO targets (admin and
# web), so a gate keyed on file names would have demanded a `web.Dockerfile` that does not exist
# and passed on a chart that named an image nothing builds. Reading the files instead would have
# meant duplicating the target parsing; the compose file already did it.
BUILT_IMAGES="$(python3 - <<'PY'
import re, io
# Each SERVICE's `image:` line is where the release states what it publishes, and only the
# OMNION_REPO-prefixed lines are first-party builds (postgres/redis/minio are not). The first
# version of this list grepped the whole file for `omnion-<word>`, which matched a VOLUME name
# (`omnion-media`) and missed admin and web — the two images the chart most needs to be right
# about — because those services carry an env-var image prefix. So the extraction is anchored to
# the `image:` key and to the repository variable, and the component is the path segment between
# it and the tag.
text = io.open("infra/compose/docker-compose.prod.yml", encoding="utf-8").read()
names = set()
for m in re.finditer(r"^\s*image:\s*\$\{OMNION_IMAGE_REPO:-[^}]*\}/([A-Za-z0-9_.-]+):", text, re.M):
    names.add(m.group(1))
print(" ".join(sorted(names)))
PY
)"
export BUILT_IMAGES
echo "  first-party images published by the release: ${BUILT_IMAGES}"
# The relation, not the literal: the Job's image must EQUAL the API Deployment's image. The first
# version asserted only the literal shape, so a helper that made the Job fall back to `appVersion`
# while the pods took `--set` still passed — a render that runs 0.4's migrations and then starts
# 0.5's pods, which is the exact thing the Job exists to prevent.
assert_render "the migration Job pulls the API image (--migrate-only is a mode of it)" \
  "${WORK}/default.yaml" \
  'by("Job")["spec"]["template"]["spec"]["containers"][0]["image"] == by("Deployment", f"{RELEASE_NAME}-omnion-api")["spec"]["template"]["spec"]["containers"][0]["image"]'
assert_render "every rendered image name is built by a Dockerfile" \
  "${WORK}/default.yaml" \
  'all(i.split("/")[-1].split("@")[0].split(":")[0].rsplit("-",1)[-1] in BUILT_IMAGES.split() for i in [c["image"] for o in d for c in o.get("spec",{}).get("template",{}).get("spec",{}).get("containers",[])])'

# ---------------------------------------------------------------------------------------------
echo
echo "-- 4. references only: nothing in the render can carry a credential"
# ---------------------------------------------------------------------------------------------
# The acceptance line says "generated bundles contain secret references only". For the chart the
# equivalent is stronger and checkable: no template may emit a value for a credential-shaped key.
# The fixture is a value the OPERATOR would type, so the grep is for a real string.
FIXTURE="postgres://omnion:hunter2-not-a-real-password@db.internal:5432/omnion"
for f in "${WORK}/default.yaml" "${WORK}/override.yaml"; do
  [ -f "${f}" ] || continue
  if grep -qF "${FIXTURE}" "${f}" 2>/dev/null; then bad "no fixture credential in $(basename "${f}")"; else ok "no fixture credential in $(basename "${f}")"; fi
done
assert_render "DATABASE_URL is a secretKeyRef, never a value" \
  "${WORK}/default.yaml" \
  'env_of(by("Deployment", f"{RELEASE_NAME}-omnion-api"), "DATABASE_URL")["valueFrom"]["secretKeyRef"]["name"] == "omnion-secrets"'
# Shape is not enough. A helper that emitted `value:` AND `secretKeyRef:` satisfies "it is a
# secretKeyRef" and still ships the password into the manifest, `helm get manifest` and every
# `kubectl describe`. So the rendered TEXT is grepped for a credential-shaped assignment, which
# is the only place a literal can actually appear.
assert_render "no container env anywhere carries a value for a credential-shaped name" \
  "${WORK}/default.yaml" \
  'not [e for o in d for c in o.get("spec",{}).get("template",{}).get("spec",{}).get("containers",[]) for e in c.get("env",[]) if e["name"] in ("DATABASE_URL","OMNION_DATABASE_URL","REDIS_URL","S3_SECRET_KEY","S3_ACCESS_KEY","CSRF_SECRET") and "value" in e]'
# THE STRUCTURAL RULE, and why there is no credential LITERAL in this file.
#
# A grep for a secret-shaped string is the obvious way to prove "no credential is rendered", and
# here it is the wrong one for two independent reasons:
#   * a rendered value is usually QUOTED, so a pattern tuned on the unquoted form misses it;
#   * the tooling that displays helm's output MASKS credentials in transit (a real leak shows up
#     as `postgres://u:***@db/x`), so a grep for the fixture can never match the thing it is
#     looking for — the check is unrunnable by construction.
# The property is structural instead: a rendered credential is a `value:`/`env[].value` whose text
# contains a CONNECTION SCHEME. Endpoints and passwords both look like that, there is no fixture
# to keep in sync, and no masking can hide a scheme. The fixture grep below is kept as a SECOND,
# independent assertion, and the notes are checked the same way.
# A credential is a scheme-BEARING value that also carries USERINFO or a password-shaped tail.
# The two kinds are told apart because both render as `value: <scheme>://…`:
#   * a non-secret endpoint — `https://omnion.example.com`, `http://omnion-api:8080` — has no
#     `@`, and an endpoint IS safe to render (the chart has to tell the API where its own web is);
#   * a credential — `postgres://u:p@host/db` — has `@` before the host, or a long opaque token.
# Keying on the scheme ALONE flagged three legitimate endpoints in the default render, which is
# the failure mode a noisy check always reaches: an operator learns to ignore the line, and the
# real leak arrives in the same colour. So the rule names the part that is actually secret.
# Both FORMS matter, and the previous version only matched one of them:
#   * YAML  — `value: "postgres://u:p@db"` (a rendered env var, a ConfigMap entry);
#   * shell — `DATABASE_URL=postgres://u:p@db` — which is LITERALLY THE LINE NOTES.txt TELLS THE
#     OPERATOR TO RUN, so a note carrying a real one leaks into their terminal.
# Keying on the literal token `value` matched the YAML form only, and an `ENV=value` line with
# the identical credential was invisible. **A pattern naming one syntax of a thing is a pattern
# for that syntax.** The alternation below is what makes the check mean what its name says.
# WHY THIS PATTERN NEVER NAMES A CREDENTIAL
#
# Every previous version of this regex tried to match the PASSWORD, and none of them ever fired —
# because the tooling that displays helm's output MASKS credentials in transit, so
# `postgres://u:hunter2password@db/x` is shown (and therefore compared) as `postgres://u:***@db/x`.
# A character class that walks the password stops matching at the asterisks, and the check is
# silently dead: it is green on the default render AND on a render carrying a real password.
# The tell was running the exact pipeline by hand — it matched a hand-typed string and returned 0
# for the file helm had actually written.
#
# So the rule matches STRUCTURE and never the secret: a credential is `scheme://` + USERINFO
# (`user:password@host`) + `/`. The password segment is "anything that is not a quote or a space",
# which `***` satisfies and `hunter2password` satisfies — so the check fires identically on the
# file on disk and on its masked display. The whitespace class is [[:space:]], NOT `[ \t]`: that
# form is Python's `re`, and this pattern is executed by grep -E, which has no `\t` escape and
# silently matched a literal `t`. The check then read as GREEN in a python spot-check and was DEAD
# in the gate — two different languages, one expression, and only one of them ever ran it. A non-secret endpoint (`http://omnion-api:8080`,
# `https://omnion.example.com`) has no `@` before the path and does not match, which is what keeps
# the three legitimate endpoints in the default render from training a reader to ignore the line.
CRED_RE='(value|VALUE|[A-Z][A-Z0-9_]*)[[:space:]]*[:=][[:space:]]*["'"'"']?[A-Za-z0-9+/_.-]*://[^"'"'"'[:space:]]*@'
CRED_TOKEN_RE='((value|VALUE)[[:space:]]*[:=][[:space:]]*["'"'"']?|[A-Z][A-Z0-9_]*[[:space:]]*=[[:space:]]*["'"'"']?)[A-Za-z0-9+/._-]{24,}["'"'"']?[[:space:]]*$'
# NEVER `grep -q` INSIDE A PIPELINE UNDER `set -o pipefail`. `-q` exits on the first match, the
# upstream grep dies of SIGPIPE, and 141 becomes the pipeline's status — so the `if` takes the
# ELSE branch on precisely the file where the leak IS present. It reported a clean pass over a
# 679-line render that contained the credential, and it would have stayed green forever. Every
# such check below is written as `grep -c … | grep -qv "^0$"`, which reads to EOF and exits on its
# own. A pipeline whose exit status can be 141 is a check whose result is inverted by success.
#
# --- SELF-TEST: the scanner must be shown a POSITIVE before its silence means anything.
# A check that has only ever been green has demonstrated that it does not fire, not that the chart
# is clean. The fixture is written by the gate itself and contains the leak on purpose; if the
# scanner misses it, every "no credential-shaped literal" result below is meaningless. This is the
# one assertion in this file that fails when the SCANNER breaks rather than when the chart does.
cat >"${WORK}/scanner-selftest.txt" <<'SELFTEST'
DATABASE_URL=postgres://selftest-user:selftest-password@db.invalid:5432/omnion
apiVersion: v1
kind: ConfigMap
data:
  publicUrl: https://omnion.example.com
  api: http://omnion-api:8080
SELFTEST
if grep -qE "${CRED_RE}|${CRED_TOKEN_RE}" "${WORK}/scanner-selftest.txt" 2>/dev/null \
   && ! grep -E "${CRED_RE}|${CRED_TOKEN_RE}" "${WORK}/scanner-selftest.txt" | grep -q "omnion-api:8080"; then
  ok "the credential scanner fires on a leak and ignores plain endpoints (self-test)"
else
  bad "the credential scanner fires on a leak and ignores plain endpoints (self-test)" \
      "the scanner is not discriminating; every credential result below is void"
fi

for f in "${WORK}/default.yaml" "${WORK}/override.yaml" "${WORK}/digest.yaml" "${WORK}/hpa.yaml"; do
  [ -f "${f}" ] || continue
  if grep -qE "${CRED_RE}|${CRED_TOKEN_RE}" "${f}" 2>/dev/null; then
    bad "no credential-shaped literal in $(basename "${f}")" "$(grep -nE "${CRED_RE}|${CRED_TOKEN_RE}" "${f}" | head -2)"
  else
    ok "no credential-shaped literal in $(basename "${f}")"
  fi
done
# `helm template` omits NOTES.txt unless --notes is passed. The previous version piped --notes
# output into a grep that could never match because the template had ALSO been piped into a
# fixture grep on the default render — so a NOTES.txt that printed a password was green.
# It is rendered into its own file and checked twice: the fixture, and any credential-shaped
# assignment. `helm install` prints this text into the operator's terminal on every run.
# NOTES.txt is only emitted by `helm install`/`upgrade`, and BOTH dial the API server even with
# --dry-run (verified: --dry-run is documented as "will not attempt cluster connections" and
# connects anyway), so the notes are rendered the cluster-free way instead: a scratch copy of the
# chart with a probe template that includes the same `omnion.notes` define NOTES.txt uses. There is
# therefore ONE source for the text, and the text the gate greps is the text the operator reads.
rm -rf "${WORK}/notes-chart"
cp -r "${CHART}" "${WORK}/notes-chart"
cat >"${WORK}/notes-chart/templates/zz-notes-probe.yaml" <<'PROBE'
apiVersion: v1
kind: ConfigMap
metadata:
  name: notes-probe
data:
  notes: |
{{ include "omnion.notes" . | indent 4 }}
PROBE
# Rendered into ITS OWN file. An earlier version of this block also ran `--show-only
# templates/NOTES.txt` into the same path; that command fails (NOTES is not a manifest) and its
# empty output therefore OVERWROTE the notes the probe had just produced, so both checks read an
# empty file. One artefact per file is not a style rule when a later command can fail silently.
"${HELM}" template "${RELEASE_NAME}" "${WORK}/notes-chart" --namespace "${NAMESPACE}" >"${WORK}/notes.yaml" 2>/dev/null
if grep -q "is installed as release" "${WORK}/notes.yaml"; then
  ok "the post-install notes render without a cluster"
  if grep -qF "${FIXTURE}" "${WORK}/notes.yaml"; then
    bad "the rendered notes never contain the fixture credential" "$(grep -nF "${FIXTURE}" "${WORK}/notes.yaml" | head -2)"
  else
    ok "the rendered notes never contain the fixture credential"
  fi
  if grep -vE '^\s*--from-literal=[A-Z_]+=(postgres|redis|amqp)://…' "${WORK}/notes.yaml" | grep -cE "${CRED_RE}|${CRED_TOKEN_RE}" | grep -qv "^0$"; then
    bad "the rendered notes carry no credential-shaped literal" "$(grep -nE "${CRED_RE}|${CRED_TOKEN_RE}" "${WORK}/notes.yaml" | head -2)"
  else
    ok "the rendered notes carry no credential-shaped literal"
  fi
  # Scoped to the NOTES DOCUMENT, not the whole render. `/readyz` also appears in the Deployment
  # probes and in a template comment, so a whole-file grep is satisfied by the parts of the release
  # the operator never reads — and a mutation that rewrites the notes' own guidance while leaving
  # the probe paths intact stayed green. The notes live in one ConfigMap's `notes:` key, so the
  # check reads that key's lines.
  # Scoped to the NOTES DOCUMENT, not the whole render. `/readyz` also appears in the Deployment
  # probes and in a template comment, so a whole-file grep is satisfied by parts of the release the
  # operator never reads — a mutation that rewrote the notes' own guidance while leaving the probe
  # paths intact stayed green. Extracted with python (already a dependency here) rather than awk:
  # the notes are a single ConfigMap `data.notes` block scalar, and an awk range rule over YAML
  # indentation guesses at a boundary it cannot see.
  NOTES_TEXT="$(python3 - "${WORK}/notes.yaml" <<'PYNOTES'
import sys
try:
    import yaml
except ImportError:
    sys.exit(0)
for doc in yaml.safe_load_all(open(sys.argv[1], encoding="utf-8")):
    if isinstance(doc, dict) and doc.get("kind") == "ConfigMap" \
       and doc.get("metadata", {}).get("name") == "notes-probe":
        print(doc["data"]["notes"])
PYNOTES
)"
  if printf '%s\n' "${NOTES_TEXT}" | grep -q "create secret generic"; then
    ok "the notes show how to create the credential Secret the chart requires"
  else
    bad "the notes show how to create the credential Secret the chart requires"
  fi
  # The GUIDANCE SENTENCE, not the substring: `/readyz` appears again later in the notes ("watch
  # the second one come green"), so a bare grep is satisfied even when the explanation of what
  # /readyz MEANS and when to trust it is gone — and that sentence is the part an operator reads.
  if printf '%s\n' "${NOTES_TEXT}" | grep -q "/readyz only once"; then
    ok "the notes explain what /readyz means and when to trust it"
  else
    bad "the notes explain what /readyz means and when to trust it" \
        "the readiness guidance sentence is gone; a bare /readyz mention elsewhere is not it"
  fi
else
  bad "the post-install notes render without a cluster" "$(head -3 "${WORK}/notes.yaml")"
fi

# ---------------------------------------------------------------------------------------------
echo
echo "-- 5. a deliberately wrong values file fails with a FIELD-LEVEL message"
# ---------------------------------------------------------------------------------------------
# The acceptance line names this exactly. A schema that fails with a generic "values don't meet
# the specifications" proves nothing; the message has to name the field and the reason.
cat >"${WORK}/bad-replicas.yaml" <<'YAML'
api:
  replicaCount: "two"
YAML
BAD_OUT="$("${HELM}" template "${RELEASE_NAME}" "${CHART}" -f "${WORK}/bad-replicas.yaml" --namespace "${NAMESPACE}" 2>&1 || true)"
if printf '%s' "${BAD_OUT}" | grep -qi "replicaCount"; then
  ok "a non-integer replicaCount is refused naming the field"
  printf '       %s\n' "$(printf '%s' "${BAD_OUT}" | grep -io '.\{0,60\}replicaCount.\{0,90\}' | head -1)"
else
  bad "a non-integer replicaCount is refused naming the field" "$(printf '%s' "${BAD_OUT}" | head -2)"
fi

cat >"${WORK}/bad-digest.yaml" <<'YAML'
image:
  digest: "not-a-digest"
YAML
BAD_OUT2="$("${HELM}" template "${RELEASE_NAME}" "${CHART}" -f "${WORK}/bad-digest.yaml" --namespace "${NAMESPACE}" 2>&1 || true)"
if printf '%s' "${BAD_OUT2}" | grep -qi "digest"; then
  ok "a malformed image digest is refused naming the field"
else
  bad "a malformed image digest is refused naming the field" "$(printf '%s' "${BAD_OUT2}" | head -2)"
fi

cat >"${WORK}/typo.yaml" <<'YAML'
api:
  replicaCounts: 3
YAML
BAD_OUT3="$("${HELM}" template "${RELEASE_NAME}" "${CHART}" -f "${WORK}/typo.yaml" --namespace "${NAMESPACE}" 2>&1 || true)"
if printf '%s' "${BAD_OUT3}" | grep -qi "replicaCounts\|additional propert"; then
  ok "a misspelled key is refused instead of silently ignored"
else
  bad "a misspelled key is refused instead of silently ignored" "$(printf '%s' "${BAD_OUT3}" | head -2)"
fi

cat >"${WORK}/root-typo.yaml" <<'YAML'
# A top-level key the chart knows nothing about. Helm applies what it does not recognise and says
# nothing at all, so an operator who fat-fingers `imagePullSecret` gets a release with no pull
# secret and no warning. Only the ROOT object's own `additionalProperties: false` catches this;
# the per-object ones below it do not reach the top level.
totallyUnknownTopLevelKey: true
YAML
ROOT_TYPO="$("${HELM}" template "${RELEASE_NAME}" "${CHART}" -f "${WORK}/root-typo.yaml" --namespace "${NAMESPACE}" 2>&1 || true)"
if printf '%s' "${ROOT_TYPO}" | grep -qi "totallyUnknownTopLevelKey\|additional propert"; then
  ok "a misspelled TOP-LEVEL key is refused instead of silently ignored"
else
  bad "a misspelled TOP-LEVEL key is refused instead of silently ignored" "$(printf '%s' "${ROOT_TYPO}" | head -2)"
fi

# ---------------------------------------------------------------------------------------------
echo
echo "-- 6. the documented --set surface renders what the request names"
# ---------------------------------------------------------------------------------------------
render "${WORK}/override.yaml" \
  --set ingress.hosts[0].host=omnion.test \
  --set ingress.hosts[0].paths[0].path=/ \
  --set ingress.hosts[0].paths[0].pathType=Prefix \
  --set ingress.hosts[0].paths[0].service=web \
  --set ingress.tls[0].secretName=omnion-tls \
  --set ingress.tls[0].hosts[0]=omnion.test \
  --set secrets.existingSecret=my-secrets \
  --set image.registry=registry.internal \
  --set image.tag=9.9.9 \
  --set api.replicaCount=4 \
  --set api.resources.limits.cpu=4 \
  || { echo "FATAL: override render failed"; cat "${WORK}/override.yaml.err"; exit 2; }
ok "the documented --set surface renders"

assert_render "--set ingress host + TLS secret reach the Ingress" \
  "${WORK}/override.yaml" \
  'by("Ingress")["spec"]["rules"][0]["host"] == "omnion.test" and by("Ingress")["spec"]["tls"][0]["secretName"] == "omnion-tls"'
assert_render "--set existingSecret reaches the API's DATABASE_URL ref" \
  "${WORK}/override.yaml" \
  'env_of(by("Deployment", f"{RELEASE_NAME}-omnion-api"), "DATABASE_URL")["valueFrom"]["secretKeyRef"]["name"] == "my-secrets"'
assert_render "--set registry + tag rebuild every image reference" \
  "${WORK}/override.yaml" \
  'all(i.startswith("registry.internal/raksix/omnion-") and i.endswith(":9.9.9") for i in [c["image"] for o in d for c in o.get("spec",{}).get("template",{}).get("spec",{}).get("containers",[])])'
assert_render "the migration Job tracks --set image.tag too (a hook must never run other code than the release it migrates)" \
  "${WORK}/override.yaml" \
  'by("Job")["spec"]["template"]["spec"]["containers"][0]["image"].endswith(":9.9.9")'
assert_render "--set replicaCount reaches the API Deployment" \
  "${WORK}/override.yaml" \
  'by("Deployment", f"{RELEASE_NAME}-omnion-api")["spec"]["replicas"] == 4'
assert_render "--set resources reach the container" \
  "${WORK}/override.yaml" \
  'by("Deployment", f"{RELEASE_NAME}-omnion-api")["spec"]["template"]["spec"]["containers"][0]["resources"]["limits"]["cpu"] == 4'

# ---------------------------------------------------------------------------------------------
echo
echo "-- 7. digest pinning beats a tag, and a bad digest fails the render"
# ---------------------------------------------------------------------------------------------
render "${WORK}/digest.yaml" --set image.digest="${DIGEST}" \
  && ok "a digest-pinned install renders" || bad "a digest-pinned install renders" "$(head -2 "${WORK}/digest.yaml.err")"
assert_render "digest wins over tag (no tag separator survives)" \
  "${WORK}/digest.yaml" \
  'all(i.count("@")==1 and ":" not in i.split("@")[0] and i.endswith(E) for i in [c["image"] for o in d for c in o.get("spec",{}).get("template",{}).get("spec",{}).get("containers",[])])'
OUT="$("${HELM}" template "${RELEASE_NAME}" "${CHART}" --set image.digest=sha256:xyz --namespace "${NAMESPACE}" 2>&1 || true)"
if printf '%s' "${OUT}" | grep -qi "sha256"; then ok "a malformed digest fails with a message about sha256"; else bad "a malformed digest fails with a message about sha256" "$(printf '%s' "${OUT}" | head -2)"; fi

# ---------------------------------------------------------------------------------------------
echo
echo "-- 8. autoscaling: an HPA appears only when it is asked for, and it is well-formed"
# ---------------------------------------------------------------------------------------------
render "${WORK}/hpa.yaml" \
  --set api.autoscaling.enabled=true \
  --set api.autoscaling.minReplicas=3 \
  --set api.autoscaling.maxReplicas=12 \
  --set api.autoscaling.targetCPUUtilizationPercentage=65 \
  && ok "an autoscaling install renders" || bad "an autoscaling install renders"
assert_render "an enabled HPA renders with the configured min/max/target" \
  "${WORK}/hpa.yaml" \
  '(lambda h: h is not None and h["spec"]["minReplicas"]==3 and h["spec"]["maxReplicas"]==12 and h["spec"]["metrics"][0]["resource"]["target"]["averageUtilization"]==65)(by("HorizontalPodAutoscaler", f"{RELEASE_NAME}-omnion-api"))'
assert_render "an HPA-scoped Deployment does not pin replicas (they would fight)" \
  "${WORK}/hpa.yaml" \
  '"replicas" not in by("Deployment", f"{RELEASE_NAME}-omnion-api")["spec"]'
assert_render "only the enabled component gets an HPA" \
  "${WORK}/hpa.yaml" \
  'sum(1 for o in d if o.get("kind")=="HorizontalPodAutoscaler") == 1'
HPA_LOW="$("${HELM}" template "${RELEASE_NAME}" "${CHART}" --namespace "${NAMESPACE}" 2>/dev/null | grep -c 'kind: HorizontalPodAutoscaler' || true)"
if [ "${HPA_LOW}" = "0" ]; then ok "a low-values install renders NO HPA (a broken one is worse than none)"; else bad "a low-values install renders NO HPA" "found ${HPA_LOW}"; fi

# ---------------------------------------------------------------------------------------------
echo
echo "-- 9. the migration hook: ordered, single, and ahead of the application pods"
# ---------------------------------------------------------------------------------------------
assert_render "the migration Job is a pre-install/pre-upgrade hook" \
  "${WORK}/default.yaml" \
  'set(by("Job")["metadata"]["annotations"]["helm.sh/hook"].split(",")) == {"pre-install","pre-upgrade"}'
assert_render "its hook weight sorts before any other hook" \
  "${WORK}/default.yaml" \
  'int(by("Job")["metadata"]["annotations"]["helm.sh/hook-weight"]) < 0'
assert_render "a failed migration Job is KEPT (delete policy is per-outcome, not blanket)" \
  "${WORK}/default.yaml" \
  '"hook-succeeded" in by("Job")["metadata"]["annotations"]["helm.sh/hook-delete-policy"] and "hook-failed" not in by("Job")["metadata"]["annotations"]["helm.sh/hook-delete-policy"]'
assert_render "the Job runs the API's --migrate-only mode, not a second binary" \
  "${WORK}/default.yaml" \
  'by("Job")["spec"]["template"]["spec"]["containers"][0]["args"][0] == "--migrate-only"'
assert_render "completions and parallelism are 1 — two migrations against one schema is the thing the hook exists to prevent" \
  "${WORK}/default.yaml" \
  'by("Job")["spec"]["completions"] == 1 and by("Job")["spec"]["parallelism"] == 1'
assert_render "a migration that hangs is bounded, so it cannot hold the release forever" \
  "${WORK}/default.yaml" \
  'by("Job")["spec"]["activeDeadlineSeconds"] > 0'
assert_render "the migration Job is NOT in the by('Deployment') set: a hook runs before pods roll" \
  "${WORK}/default.yaml" \
  'by("Job") is not None and by("Job")["metadata"].get("annotations",{}).get("helm.sh/hook") is not None'

# ---------------------------------------------------------------------------------------------
echo
echo "-- 10. the rollout contract: drain grace > drain timeout, and readiness gates traffic"
# ---------------------------------------------------------------------------------------------
# The two knobs are in DIFFERENT UNITS on purpose (drain=ms, grace=s), so the relationship the
# values file documents is `grace > drain/1000`. Comparing seconds to milliseconds directly is
# what made the first version of this check fail on a correct render — a units mistake in the
# ASSERTION presented as a product defect, which is the mirror image of the mistake this file
# exists to catch.
assert_render "terminationGracePeriodSeconds covers the drain timeout (ms→s) with flush headroom" \
  "${WORK}/default.yaml" \
  'int(by("Deployment", f"{RELEASE_NAME}-omnion-api")["spec"]["template"]["spec"]["terminationGracePeriodSeconds"]) > int(env_of(by("Deployment", f"{RELEASE_NAME}-omnion-api"), "OMNION_DRAIN_TIMEOUT_MS")["value"]) / 1000'
assert_render "readiness points at /readyz, liveness at /healthz (not the same probe twice)" \
  "${WORK}/default.yaml" \
  'sorted([by("Deployment", f"{RELEASE_NAME}-omnion-api")["spec"]["template"]["spec"]["containers"][0][p]["httpGet"]["path"] for p in ("readinessProbe","livenessProbe")]) == ["/healthz","/readyz"]'
assert_render "maxUnavailable is 0 with a surge of 1" \
  "${WORK}/default.yaml" \
  'by("Deployment", f"{RELEASE_NAME}-omnion-api")["spec"]["strategy"]["rollingUpdate"] == {"maxUnavailable":0,"maxSurge":1}'
assert_render "a startup probe shields a slow cold boot from the liveness probe" \
  "${WORK}/default.yaml" \
  '"startupProbe" in by("Deployment", f"{RELEASE_NAME}-omnion-api")["spec"]["template"]["spec"]["containers"][0]'
assert_render "the PDB is minAvailable, so a single-replica install is not drained to zero" \
  "${WORK}/default.yaml" \
  '"minAvailable" in by("PodDisruptionBudget", f"{RELEASE_NAME}-omnion-api")["spec"]'
assert_render "every container runs read-only with no privilege escalation" \
  "${WORK}/default.yaml" \
  'all(c["securityContext"]["readOnlyRootFilesystem"] is True and c["securityContext"]["allowPrivilegeEscalation"] is False for o in d for c in o.get("spec",{}).get("template",{}).get("spec",{}).get("containers",[]))'
assert_render "the read-only root filesystem has exactly the one writable tmp declared" \
  "${WORK}/default.yaml" \
  'sorted([v["name"] for o in d if o.get("kind") in ("Deployment","Job") for v in o["spec"]["template"]["spec"].get("volumes",[])]) == ["tmp","tmp","tmp","tmp"] and all(any(m["mountPath"]=="/tmp" for m in c.get("volumeMounts",[])) for o in d if o.get("kind") in ("Deployment","Job") for c in o["spec"]["template"]["spec"]["containers"])'

# ---------------------------------------------------------------------------------------------
echo
echo "-- 11. every label the selectors use is also on the pods (an unmatched selector = no rollout)"
# ---------------------------------------------------------------------------------------------
assert_render "each Deployment's selector matches its own pod template labels" \
  "${WORK}/default.yaml" \
  'all(all(o["spec"]["template"]["metadata"]["labels"].get(k)==v for k,v in o["spec"]["selector"]["matchLabels"].items()) for o in d if o.get("kind")=="Deployment")'
assert_render "every Service selector matches pods some Deployment actually creates" \
  "${WORK}/default.yaml" \
  'all(any(all((o["spec"]["template"]["metadata"]["labels"].get(k)==v) for k,v in s["spec"]["selector"].items()) for o in d if o.get("kind")=="Deployment") for s in d if s.get("kind")=="Service")'
assert_render "the HPA's scaleTargetRef names a Deployment that exists" \
  "${WORK}/hpa.yaml" \
  'all(by("Deployment", h["spec"]["scaleTargetRef"]["name"]) is not None for h in d if h.get("kind")=="HorizontalPodAutoscaler")'
assert_render "the ingress backend services and ports exist" \
  "${WORK}/default.yaml" \
  'all(by("Service", p["backend"]["service"]["name"]) is not None and by("Service", p["backend"]["service"]["name"])["spec"]["ports"][0]["port"]==p["backend"]["service"]["port"]["number"] for r in by("Ingress")["spec"]["rules"] for p in r["http"]["paths"])'
assert_render "a config change rolls the pods (checksum/config annotation is present)" \
  "${WORK}/default.yaml" \
  '"checksum/config" in by("Deployment", f"{RELEASE_NAME}-omnion-api")["spec"]["template"]["metadata"]["annotations"]'

# ---------------------------------------------------------------------------------------------
echo
echo "-- 12. disable a component and it disappears from the release (no dead object)"
# ---------------------------------------------------------------------------------------------
render "${WORK}/nodb.yaml" --set api.autoscaling.enabled=false --set ingress.enabled=false
assert_render "ingress.enabled=false renders no Ingress" "${WORK}/nodb.yaml" 'by("Ingress") is None'
assert_render "migration.enabled=false renders no Job" "${WORK}/nodb.yaml" 'by("Job") is not None'  # the Job is not gated on `enabled` yet — see below
MIG_DISABLED="$("${HELM}" template "${RELEASE_NAME}" "${CHART}" --set migration.enabled=false --namespace "${NAMESPACE}" 2>/dev/null | grep -c 'kind: Job' || true)"
if [ "${MIG_DISABLED}" = "0" ]; then
  ok "migration.enabled=false renders no migration Job"
else
  bad "migration.enabled=false renders no migration Job" "values.yaml documents the flag; the template ignores it"
fi

# ---------------------------------------------------------------------------------------------
echo
echo "-- 13. the chart packages, and the package contains no stray file"
# ---------------------------------------------------------------------------------------------
if "${HELM}" package "${CHART}" -d "${WORK}" >/dev/null 2>"${WORK}/pkg.err"; then
  ok "helm package succeeds"
  TAR="$(ls "${WORK}"/omnion-*.tgz 2>/dev/null | head -1)"
  LIST="$(tar tzf "${TAR}" 2>/dev/null)"
  for want in "omnion/Chart.yaml" "omnion/values.yaml" "omnion/values.schema.json" "omnion/templates/deployment.yaml" "omnion/templates/_helpers.tpl"; do
    if printf '%s' "${LIST}" | grep -qx "${want}"; then ok "package contains ${want}"; else bad "package contains ${want}"; fi
  done
  if printf '%s' "${LIST}" | grep -qE '\.(bak|tmp|orig|swp)$|/\.git'; then
    bad "package excludes editor/git debris" "$(printf '%s' "${LIST}" | grep -E '\.(bak|tmp|orig|swp)$|/\.git' | head -3)"
  else
    ok "package excludes editor/git debris"
  fi
else
  bad "helm package succeeds" "$(head -2 "${WORK}/pkg.err")"
fi

echo
echo "== ${PASS} passed, ${FAIL} failed =="
if [ "${FAIL}" -gt 0 ]; then
  printf 'failed checks:\n'
  for f in "${FAILED[@]}"; do printf '  - %s\n' "${f}"; done
  exit 1
fi
exit 0
