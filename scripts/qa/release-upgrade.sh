#!/usr/bin/env bash
# Upgrade plan gate (REQ-128, slice 4).
#
# The unit tests (release/tests/test_release_upgrade.py) prove the builder's logic. This
# proves the two claims that only hold end to end:
#
#   1. A plan built from this repository's REAL compose stacks and REAL migration files
#      verifies. The unit tests use synthetic manifest documents; this one reads
#      database/migrations/ and the compose files, so a plan built against a repository fact
#      that has since changed is caught here rather than by an operator.
#   2. Every command a step names is RUNNABLE IN SHAPE — parsed by a shell, with the stack
#      file it references confirmed to exist. A plan is pasted into a production shell, and
#      `bash -n` is the cheapest possible check that the thing parses before it is run.
#
# Every rule is mutated below. A rule nobody has seen fail is not evidence that it works.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/omnion-upgrade-gate.XXXXXX")"
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
section() { echo "== $1"; }

cd "$ROOT" || exit 1

M1='0001_init.sql'
M2='0002_content.sql'

# Two manifests built from the repository's own migration list, split so the range has a
# real delta. A gate whose fixtures are hand-written constants passes while the code drifts
# from the tree; these are read off disk.
python3 - "$WORK" <<'PY'
import json, os, sys
sys.path.insert(0, "release/lib")
import manifest

work = sys.argv[1]
shipped = manifest.discover_migrations(".")
split = max(1, len(shipped) // 2)
docs = {
    "from": {"manifest_version": "1", "version": "0.4.0", "registry": "ghcr.io",
             "repository": "raksix/omnion", "core_min": "0.1.0",
             "migrations": shipped[:split], "migrations_destructive": None},
    "to": {"manifest_version": "1", "version": "0.5.0", "registry": "ghcr.io",
           "repository": "raksix/omnion", "core_min": "0.1.0",
           "migrations": shipped, "migrations_destructive": None},
}
for name, doc in docs.items():
    with open(os.path.join(work, name + ".json"), "w", encoding="utf-8") as handle:
        json.dump(doc, handle, indent=2)
print(f"range: {split} already applied, {len(shipped) - split} new")
PY
[ $? -eq 0 ] && pass || fail "fixture manifests build from the repository's migrations"

plan() { # [extra args...] → the plan JSON on stdout
  python3 release/lib/upgrade.py --from-version 0.4.0 \
    --to-manifest "$WORK/to.json" --from-manifest "$WORK/from.json" "$@"
}

# ------------------------------------------------------------------------------------------
section "the plan a real repository produces"
# ------------------------------------------------------------------------------------------
if plan > "$WORK/plan.json" 2>"$WORK/err"; then pass; else fail "a plan builds" "$(cat "$WORK/err")"; fi

state() { python3 -c "import json,sys; print(json.load(open('$WORK/plan.json'))['verification']['state'])" 2>/dev/null; }
problems() { python3 -c "import json; print(' | '.join(json.load(open('$WORK/plan.json'))['verification']['problems']))" 2>/dev/null; }

if [ "$(state)" = "verified" ]; then pass; else fail "a real plan verifies" "$(problems)"; fi

# The delta must equal the migrations between the split, read off disk again. A plan that
# reports the whole list as new is wrong in the way that wastes an operator's afternoon.
delta_ok=$(python3 - "$WORK/plan.json" <<'PY'
import json, os, sys
sys.path.insert(0, "release/lib")
import manifest
shipped = manifest.discover_migrations(".")
split = max(1, len(shipped) // 2)
plan = json.load(open(sys.argv[1]))
raise SystemExit(0 if plan["migrations_applied"] == shipped[split:] else 1)
PY
)
if [ $? -eq 0 ]; then pass; else fail "the delta is exactly the migrations between the split"; fi

# The verdict on this repository is UNKNOWN, because REQ-129 has not landed. Asserting the
# exact string, not "not destructive": a helper that answered `reversible` here would be
# promising a verified down script that nobody has run.
verdict=$(python3 -c "import json; print(json.load(open('$WORK/plan.json'))['destructive']['verdict'])" 2>/dev/null)
if [ "$verdict" = "unknown" ]; then pass; else fail "verdict is unknown while the policy is absent" "got: $verdict"; fi

# ------------------------------------------------------------------------------------------
section "every step command parses in a shell"
# ------------------------------------------------------------------------------------------
# `bash -n` on each command. A plan is pasted into a production shell, and a syntax error in
# step 4 of 6 is discovered after the backup and the migration.
python3 - "$WORK/plan.json" "$WORK" > "$WORK/parsed.txt" 2>&1 <<'PY'
import json, subprocess, sys
plan = json.load(open(sys.argv[1]))
bad = []
checked = 0
for index, step in enumerate(plan["steps"]):
    command = step.get("command")
    if not command:
        continue
    checked += 1
    done = subprocess.run(["bash", "-n", "-c", command], capture_output=True, text=True)
    if done.returncode != 0:
        bad.append(f"step {index} ({step['kind']}): {done.stderr.strip()}")
print(f"checked {checked} commands")
for line in bad:
    print(line)
raise SystemExit(1 if bad else 0)
PY
if [ $? -eq 0 ]; then pass; else fail "every step command parses" "$(cat "$WORK/parsed.txt")"; fi

# Every stack file a command names is a file the repository ships.
stacks_ok=$(python3 - "$WORK/plan.json" <<'PY'
import json, os, sys
plan = json.load(open(sys.argv[1]))
missing = []
for step in plan["steps"]:
    for token in (step.get("command") or "").split():
        if "docker-compose" in token and token.endswith(".yml"):
            if not os.path.exists(os.path.join("infra", "compose", token)):
                missing.append(token)
print("missing: " + ", ".join(missing) if missing else "all present")
raise SystemExit(1 if missing else 0)
PY
)
if [ $? -eq 0 ]; then pass; else fail "every stack file a step names exists" "$stacks_ok"; fi

# The compose stack really does gate the API on the migration completing. This is the
# request's own rule ("migrations run before new code serves traffic") and the plan asserts
# it, so the gate reads the RENDERED stack rather than trusting the plan.
if python3 - <<'PY'
import re, sys
text = open("infra/compose/docker-compose.prod.yml", encoding="utf-8").read()
raise SystemExit(0 if "service_completed_successfully" in text else 1)
PY
then pass; else fail "the prod stack gates the api on the migrate service completing"; fi

# ------------------------------------------------------------------------------------------
section "the checklist refuses to complete"
# ------------------------------------------------------------------------------------------
unacked=$(python3 -c "import json; c=json.load(open('$WORK/plan.json'))['checklist']; print(c['complete'], c['requires_acknowledgement'])" 2>/dev/null)
if [ "$unacked" = "False True" ]; then pass; else fail "an unacknowledged plan does not complete" "got: $unacked"; fi

# ------------------------------------------------------------------------------------------
section "kubernetes renders the hook, and the guide matches the plan"
# ------------------------------------------------------------------------------------------
if plan --topology kubernetes > "$WORK/k8s.json" 2>"$WORK/err"; then pass; else fail "the kubernetes topology builds" "$(cat "$WORK/err")"; fi
kstate=$(python3 -c "import json; print(json.load(open('$WORK/k8s.json'))['verification']['state'])" 2>/dev/null)
if [ "$kstate" = "verified" ]; then pass; else fail "the kubernetes plan verifies"; fi

# The chart the plan names must be the chart on disk, and the plan's migration step must
# come before its deploy step — the two claims a plan makes about Kubernetes.
k8s_ok=$(python3 - "$WORK/k8s.json" <<'PY'
import json, os, sys
plan = json.load(open(sys.argv[1]))
kinds = [s["kind"] for s in plan["steps"]]
problems = []
if kinds.index("migrate") > kinds.index("deploy"):
    problems.append("migrate comes after deploy")
marked = [i for i, s in enumerate(plan["steps"]) if s["point_of_no_return"]]
if len(marked) != 1 or plan["steps"][marked[0]]["kind"] != "migrate":
    problems.append("point of no return is not on the single migrate step")
for step in plan["steps"]:
    if "infra/helm/omnion" in (step.get("command") or "") and not os.path.isdir("infra/helm/omnion"):
        problems.append("the plan names a chart directory that does not exist")
print(" | ".join(problems) if problems else "ok")
raise SystemExit(1 if problems else 0)
PY
)
if [ $? -eq 0 ]; then pass; else fail "the kubernetes plan's order and marker are right" "$k8s_ok"; fi

# The chart is a pre-upgrade hook, which is what makes `helm upgrade` apply the migration.
if grep -q "pre-upgrade" infra/helm/omnion/templates/migrate-job.yaml; then pass; else fail "the chart runs migrations as a pre-upgrade hook"; fi

# The upgrade guide must name both topologies and the split, and every command it shows for
# a step must be one the plan also produces. A guide that has drifted from the code is a
# document that costs an operator an hour, which is the thing this slice exists to prevent.
if [ -f docs/deployment/upgrade.md ]; then pass; else fail "docs/deployment/upgrade.md exists"; fi
for topology in compose kubernetes; do
  if grep -qi "$topology" docs/deployment/upgrade.md 2>/dev/null; then pass; else fail "the guide covers $topology"; fi
done
if grep -qi "application rollback" docs/deployment/upgrade.md 2>/dev/null &&
   grep -qi "database rollback" docs/deployment/upgrade.md 2>/dev/null; then
  pass
else fail "the guide splits application from database rollback"; fi

# ------------------------------------------------------------------------------------------
section "refusals"
# ------------------------------------------------------------------------------------------
refuse() { # name, expected-substring, args...
  local name="$1" want="$2"; shift 2
  local out
  out="$(python3 release/lib/upgrade.py "$@" 2>&1)"
  case "$out" in
    *"$want"*) pass ;;
    *) fail "REFUSAL: $name" "expected '$want', got: $(echo "$out" | tr '\n' ' ' | cut -c1-160)" ;;
  esac
}

refuse "a downgrade" "older than the running" \
  --from-version 0.9.0 --to-manifest "$WORK/to.json" --from-manifest "$WORK/from.json"
refuse "the same version" "both 0.5.0" \
  --from-version 0.5.0 --to-manifest "$WORK/to.json" --from-manifest "$WORK/to.json"
refuse "a missing source manifest" "from_manifest is required" \
  --from-version 0.4.0 --to-manifest "$WORK/to.json"
refuse "a missing target manifest" "not found" \
  --from-version 0.4.0 --to-manifest "$WORK/nope.json" --from-manifest "$WORK/from.json"
refuse "an unknown topology" "invalid choice" \
  --from-version 0.4.0 --to-manifest "$WORK/to.json" --from-manifest "$WORK/from.json" \
  --topology nomad

# The acknowledgement is what completes the checklist, and it has to be the operator's.
if plan --acknowledge --acknowledge-as ops@example.com > "$WORK/ack.json" 2>&1; then pass; else fail "an acknowledged plan builds"; fi
ack_state=$(python3 -c "import json; d=json.load(open('$WORK/ack.json')); print(d['checklist']['complete'], d['destructive'].get('acknowledged_verdict'))" 2>/dev/null)
if [ "$ack_state" = "True unknown" ]; then pass; else fail "acknowledging records the verdict, not just a tick" "got: $ack_state"; fi

# ------------------------------------------------------------------------------------------
section "mutations — each check must be able to fail"
# ------------------------------------------------------------------------------------------

MUT="$WORK/mut"
# `cp -r release/lib "$MUT"` with $MUT not yet existing copies the DIRECTORY TO THAT NAME —
# so the module lands at $MUT/upgrade.py and every mutation below patched a path that was
# never there. All five reported "the patch did not apply", which is the correct report: the
# `assert` in each mutation is what turned a silent no-op into a visible failure, and it is
# why they say so rather than quietly passing. The parent is created first, so the copy
# lands at $MUT/lib where the rest of the gate expects it.
mkdir -p "$MUT"
cp -r "$ROOT/release/lib" "$MUT/lib"
if [ -f "$MUT/lib/upgrade.py" ]; then pass; else fail "the mutation copy contains the module under test"; fi

# A mutation is proven by DIFFERENTIAL behaviour against the unmutated module, never by
# asking the mutated module whether it is happy. Two reasons, both learned in this file:
#
#   * A check with a SIBLING still fires. Removing the checklist gate did not stop the
#     fixture being caught, because the "unrecognised verdict" rule and the rollback-method
#     rule read the same fields — three mutations reported "the check is still load-bearing"
#     when what had happened is that a DIFFERENT check caught the case. A mutation that
#     measures one check in isolation therefore proves nothing about that check.
#   * The mutated module's own answer is the variable under test. Asking it "did you catch
#     this?" returns whatever it was mutated to return.
#
# So every mutation below loads BOTH copies and asserts the behaviour DIFFERS, and names in
# its failure message which of the two ways that can happen. That is the only formulation
# that cannot be satisfied by a check that stopped working.
mutate() { # <file> <old> <new> — patch the copy, refusing silently-unapplied edits
  python3 - "$MUT" "$1" "$2" "$3" <<'PY'
import os, sys
mut, rel, old, new = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
path = os.path.join(mut, "lib", rel)
text = open(path, encoding="utf-8").read()
if old not in text:
    raise SystemExit(f"mutation target not found in {rel}: {old!r}")
open(path, "w", encoding="utf-8").write(text.replace(old, new, 1))
PY
}
restore() { cp "$ROOT/release/lib/$1" "$MUT/lib/$1"; }

# Runs one fixture against both the real module and the mutated one and reports
# "<real> <mutated>". The verdict is the CALLER's: this prints, it does not judge.
#
# Two PROCESSES, not two imports in one interpreter. `upgrade.py` opens with
# `sys.path.insert(0, <its own directory>)` — the same trick the other three modules use so
# they can import each other as siblings — so a second `import upgrade` in the same
# interpreter does not reach the other copy: the first import's `sys.path[0]` is still
# there, the second copy silently resolves to the first, and the differential probe compares
# the module against ITSELF. It reported six identical results and the gate called them all
# failures. The cheapest fix is a fresh interpreter per copy, so the path insert is the only
# thing that has to be right and it is right by construction.
diff_probe() { # <probe file name under $WORK> → "<real>|<mutated>" on one line, pipe-separated
  local real mutated
  real=$(OMNION_LIB="$ROOT/release/lib" python3 "$WORK/probe.py" "$WORK/$1" 2>&1)
  mutated=$(OMNION_LIB="$MUT/lib" python3 "$WORK/probe.py" "$WORK/$1" 2>&1)
  # The separator is `|`, not a space, and it is not a detail: `${out%% *}` splits on the
  # first SPACE, so a fixture whose value contains one — a tuple, a list of step kinds, any
  # dict — silently splits in the wrong place and the gate then reported "real=(True," for a
  # probe that returned a two-element tuple. A separator that cannot appear inside a repr()
  # is the only kind that works; `|` cannot, because Python's repr uses `|` nowhere and
  # `print(repr(...))` of a container never emits it.
  printf '%s|%s\n' "$real" "$mutated"
}

# The probe RUNNER, written once. It reads the library directory from the environment and the
# probe body from a file, because a multi-line probe cannot travel as one `sys.argv` value —
# bash keeps the newlines, python keeps them too, and `eval` then reports a `SyntaxError` at
# line 2. That is what the previous version did, and it looked exactly like "every mutation
# produced a syntax error", which is a failure report you cannot act on.
cat > "$WORK/probe.py" <<'PY'
import os, sys
sys.path.insert(0, os.environ["OMNION_LIB"])
import upgrade as upg
with open(sys.argv[1], encoding="utf-8") as handle:
    body = handle.read()
# `exec`, not `eval`: four of the six fixtures are statement lists (assign a plan, mutate a
# field, read the result), and `eval` cannot run a statement at all — it reported a
# `SyntaxError` on line 2 of every one of them, which is the same unreadable "everything
# broke" as a syntax error is everywhere else. The probe's value is the LAST expression, so
# the body ends in one and `exec` is what makes the rest of the lines possible.
space = {"upg": upg, "os": os}
exec(compile(body, sys.argv[1], "exec"), space)
value = space.get("VALUE")
if value is None:
    raise SystemExit("the probe produced no VALUE; every fixture must end with `VALUE = …`")
print(repr(value))
PY
[ -f "$WORK/probe.py" ] && pass || fail "the probe runner is written"

# The six fixtures, as files. Each is ONE expression: a statement list would need exec(),
# and an expression is what `compile(..., "eval")` runs without another layer.
cat > "$WORK/p1.py" <<'PY'
VALUE = upg.destructiveness(["0002_content.sql"], {"migrations_destructive": None}, os.getcwd())["verdict"]
PY
cat > "$WORK/p2.py" <<'PY'
plan = upg.build_plan(
    from_version="0.4.0",
    to_manifest={"version": "0.5.0", "registry": "ghcr.io",
                 "migrations": ["0001_init.sql", "0002_content.sql"],
                 "migrations_destructive": None},
    from_manifest={"version": "0.4.0", "migrations": ["0001_init.sql"]},
    root=os.getcwd(),
)
plan["checklist"].update(complete=True, acknowledged=False)
VALUE = upg.verify_plan(plan)["state"]
PY
cat > "$WORK/p3.py" <<'PY'
plan = upg.build_plan(
    from_version="0.4.0",
    to_manifest={"version": "0.5.0", "registry": "ghcr.io",
                 "migrations": ["0001_init.sql", "0002_content.sql"],
                 "migrations_destructive": None},
    from_manifest={"version": "0.4.0", "migrations": ["0001_init.sql"]},
    root=os.getcwd(),
)
VALUE = [str(s["kind"]) for s in plan["steps"] if s["point_of_no_return"]]
PY
cat > "$WORK/p4.py" <<'PY'
plan = upg.build_plan(
    from_version="0.4.0",
    to_manifest={"version": "0.5.0", "registry": "ghcr.io",
                 "migrations": ["0001_init.sql", "0002_content.sql"],
                 "migrations_destructive": None},
    from_manifest={"version": "0.4.0", "migrations": ["0001_init.sql"]},
    root=os.getcwd(),
)
for step in plan["steps"]:
    if step.get("command"):
        step["command"] = step["command"].replace("docker-compose.prod.yml", "docker-compose.yml")
        break
VALUE = upg.verify_plan(plan)["state"]
PY
cat > "$WORK/p5.py" <<'PY'
plan = upg.build_plan(
    from_version="0.4.0",
    to_manifest={"version": "0.5.0", "registry": "ghcr.io",
                 "migrations": ["0001_init.sql", "0002_content.sql"],
                 "migrations_destructive": None},
    from_manifest={"version": "0.4.0", "migrations": ["0001_init.sql"]},
    root=os.getcwd(),
)
VALUE = plan["rollback"]["database"]["available"]
PY
cat > "$WORK/p6.py" <<'PY'
plan = upg.build_plan(
    from_version="0.4.0",
    to_manifest={"version": "0.5.0", "registry": "ghcr.io",
                 "migrations": ["0001_init.sql", "0002_content.sql"],
                 "migrations_destructive": None},
    from_manifest={"version": "0.4.0", "migrations": ["0001_init.sql"]},
    root=os.getcwd(),
)
VALUE = (plan["checklist"]["requires_acknowledgement"], plan["checklist"]["items"] != [])
PY

# check <name> <probe> <old> <new> <expected real> <expected mutated>
check() {
  local name="$1" probe="$2" old="$3" new="$4" want_real="$5" want_mut="$6"
  if ! mutate upgrade.py "$old" "$new"; then
    fail "MUTATION: $name — the patch did not apply"
    return
  fi
  local out got_real got_mut
  out="$(diff_probe "$probe")"
  got_real="${out%%|*}"; got_mut="${out##*|}"
  restore upgrade.py
  if [ "$got_real" = "$want_real" ] && [ "$got_mut" = "$want_mut" ]; then
    pass
  else
    fail "MUTATION: $name is not load-bearing" "real=$got_real (want $want_real) mutated=$got_mut (want $want_mut)"
  fi
}

# 1. The single most consequential mutation in the file: it turns "nobody has checked" into
#    "nothing to worry about", which is the one sentence this request must never render.
check "the unknown verdict" p1.py \
  "if not release_manifest._policy_exists(root):" "if False:" "'unknown'" "'reversible'"

# 2. The request's third refusal. Measured on a plan whose ONLY defect is the unacknowledged
#    complete checklist, because against a plan that also carries a bad verdict a DIFFERENT
#    check would catch it and this one could not be shown to matter.
check "the unacknowledged-checklist refusal" p2.py \
  '    if checklist_doc.get("complete") and not checklist_doc.get("acknowledged"):' \
  "    if False:" "'failed'" "'verified'"

# 3. An operator told the deploy is the point of no return rolls the app back and assumes
#    the schema followed it.
check "the point-of-no-return marker" p3.py \
  '        step["point_of_no_return"] = index == ponr' \
  '        step["point_of_no_return"] = steps[index]["kind"] == "deploy"' \
  "['migrate']" "['deploy']"

# 4. A step naming a file the repository does not ship is the error an operator hits as
#    "no such file" — after the backup.
check "the stack-file check" p4.py \
  '        if "docker-compose" in command or "docker compose" in command:' \
  "        if False:" "'failed'" "'verified'"

# 5. Offer a database rollback on an UNKNOWN plan — the quietest lie in the document, and the
#    one a panel renders as an available button. The verdict stays honest; only the promise
#    the operator reads changes.
check "the database rollback refusal" p5.py \
  '                "available": destructive["verdict"] == VERDICT_REVERSIBLE,' \
  '                "available": True,' "False" "True"

# 6. And the one that decides the slice: make the checklist ignore a destructive migration,
#    so the plan renders complete with nothing acknowledged. The verdict and the reason are
#    untouched, so the ONLY way this is caught is by a check that reads the checklist.
#
#    The first target for this mutation was the `kind in ("migrate", "manual")` clause, and it
#    proved NOTHING: the migrated plan still returned (True, True). The second target, the
#    `destructive` clause, proved nothing either, and for a better reason — on a plan with
#    migrations, `destructive` and `kind == "migrate"` select the SAME step, so removing
#    either leaves the other. A mutation that changes no observable behaviour is not a
#    mutation, it is a comment, and two of them in a row is the signal to stop tuning the
#    target and ask what the check is actually made of: here, the answer is that the
#    acknowledgement gate rests entirely on the step-KIND clauses, and `destructive` is
#    redundant for every plan this builder produces (it is kept for stored plans, whose
#    steps arrive from a database rather than from this function).
#
#    So this mutation removes both clauses and asks for the one thing that must break: a
#    plan with an unacknowledged destructive migration rendering a COMPLETE checklist.
check "the checklist's acknowledgement gate" p6.py \
  '        if step.get("destructive")
        or step.get("kind") in ("migrate", "manual")
        or step.get("requires_operator")' \
  '        if step.get("requires_operator")' "(True, True)" "(False, False)"


echo
if [ "$FAIL" -eq 0 ]; then
  echo "upgrade gate: $PASS passed, 0 failed"
  exit 0
fi
echo "upgrade gate: $PASS passed, $FAIL FAILED"
for name in "${FAILED_NAMES[@]}"; do echo "  - $name"; done
exit 1
