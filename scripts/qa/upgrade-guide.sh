#!/usr/bin/env bash
# Upgrade guide gate (REQ-128) — does `docs/deployment/upgrade.md` still describe this
# repository?
#
# ## Why this gate exists at all
#
# `release-upgrade.sh` already gates the *helper* — the plan builder, the refusals, the
# mutations. What it did not gate was the document an operator actually reads. Its four guide
# checks were:
#
#     grep -qi "compose"           docs/deployment/upgrade.md
#     grep -qi "kubernetes"        docs/deployment/upgrade.md
#     grep -qi "application rollback" ...
#     grep -qi "database rollback" ...
#
# Four word greps. Three properties are worth stating precisely, because each one was true:
#
#   * A grep for the word "compose" cannot tell whether the guide's COMPOSE section is
#     correct, only that the word appears somewhere in 9 KB of prose.
#   * Deleting every command from the guide leaves all four checks green. The document could
#     be a title and a sentence and it would pass.
#   * Nothing in it asked the MODULE what it answers. The guide said, in three places, that
#     REQ-129's reversal gate "has not been built" and that "every plan today reports
#     `unknown`". Both were false on the day this file was written: the policy migration
#     exists, and the module answers `destructive` / `restore-from-backup` for 54 of the 61
#     migrations this repository ships. A guide that tells an operator to expect `unknown`
#     when the panel renders `destructive` is worse than no guide, because the operator
#     reads the panel afterwards and has to work out which document is lying.
#
# So this gate reads the SAME sources the module reads and compares the guide's claims to the
# module's answers. It is built out of claims that fail when the world changes: if REQ-129's
# policy is removed, the verdict check goes red; if a down script is added to a migration, the
# census check goes red and the guide must be re-counted.
#
# ## Shape, and why it is this shape
#
# `check_guide <path>` holds every guide claim and takes the document to judge. The gate then
# runs it against the real guide and, in the mutation section, against deliberately broken
# COPIES in a temp directory. The first version of this file mutated the real file and tried
# to restore it afterwards — which is a gate that can leave the repository broken, and a gate
# whose own mutation is not reversible is not a gate I would run unattended. A parameter is
# cheaper than a restore.
#
# Every rule below is mutated at the end of the file. A rule nobody has seen fail is not
# evidence that it works, and the rules this file replaces were four greps that never had.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/omnion-guide-gate.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
cd "$ROOT" || exit 1

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

REAL_GUIDE="docs/deployment/upgrade.md"

# -------------------------------------------------------------------------------------------
# Facts about the TREE, read from disk. Computed once: the mutation section changes the
# GUIDE, never the migrations, so these are true for every run of check_guide below.
# -------------------------------------------------------------------------------------------
if ! FACTS="$(python3 - <<'PY'
import json, sys
sys.path.insert(0, "release/lib")
import manifest as m
import upgrade as upg

root = m.repo_root()
shipped = sorted(set(m.discover_migrations(root)))
unreversible = set(m.unreversible_migrations(root))
declared = set(m.declared_irreversible(root))
with_down = [n for n in shipped if n not in unreversible]
without_down = [n for n in shipped if n in unreversible]

# The verdict the helper actually renders for a real range on this tree.
delta = shipped[-3:] if len(shipped) > 3 else shipped
doc = upg.destructiveness(delta, {"version": "0.5.0", "migrations_destructive": None}, root=root)

print(json.dumps({
    "total": len(shipped),
    "with_down": len(with_down),
    "without_down": len(without_down),
    "policy_exists": m._policy_exists(root),
    "declared": len(declared),
    "verdict": doc["verdict"],
    "source": doc["source"],
    "db_rollback": doc["database_rollback"],
    "ponr": upg.point_of_no_return(delta, doc),
}))
PY
)"; then
  echo "upgrade guide gate: the module could not be loaded — aborting" >&2
  exit 1
fi

field() { python3 -c "import json,sys; print(json.loads(sys.argv[2])['$1'])" "$1" "$FACTS" 2>/dev/null; }
TOTAL_MIGS="$(field total)"
WITH_DOWN="$(field with_down)"
WITHOUT_DOWN="$(field without_down)"
VERDICT="$(field verdict)"
VERDICT_SOURCE="$(field source)"
DB_ROLLBACK="$(field db_rollback)"
POLICY_EXISTS="$(field policy_exists)"
PONR="$(field ponr)"

if [ -z "$TOTAL_MIGS" ] || [ -z "$VERDICT" ]; then
  echo "upgrade guide gate: the module produced no verdict — aborting" >&2
  exit 1
fi

# -------------------------------------------------------------------------------------------
# Every claim about the guide. $1 = path to the document under test.
# -------------------------------------------------------------------------------------------
check_guide() {
  local guide="$1" label="${2:-the guide}"
  local detail=""

  # --- it is a document with commands in it -----------------------------------------------
  local usable
  usable="$(python3 - "$guide" <<'PY'
import re, subprocess, sys
guide = sys.argv[1]
text = open(guide, encoding="utf-8").read()
blocks = re.findall(r"```bash\n(.*?)```", text, re.S)
count = 0
for index, block in enumerate(blocks):
    body = "\n".join(l for l in block.splitlines() if not l.strip().startswith("#")).strip()
    if not body:
        continue          # a prose-only block is not a command
    count += 1
    result = subprocess.run(["bash", "-n"], input=body, capture_output=True, text=True)
    if result.returncode != 0:
        print(f"UNPARSEABLE_BLOCK_{index}: {result.stderr.strip().splitlines()[0][:120]}", file=sys.stderr)
        sys.exit(3)
print(count)
PY
)" || { fail "$label: every bash command it shows parses" "$(python3 -c "
import re,sys
t=open('$guide',encoding='utf-8').read()
print('a block does not parse')")"; return; }

  if [ "$usable" -ge 4 ]; then
    pass
  else
    fail "$label: shows runnable commands for both topologies" "found $usable, want >= 4"
  fi

  # --- the census it quotes is the census on disk -----------------------------------------
  if grep -q "${WITHOUT_DOWN} of ${TOTAL_MIGS} migrations" "$guide" 2>/dev/null ||
     grep -q "\*\*${WITHOUT_DOWN} of them carry no down" "$guide" 2>/dev/null; then
    pass
  else
    fail "$label: migration census matches the tree" \
         "tree says ${WITHOUT_DOWN} of ${TOTAL_MIGS} ship no down script"
  fi

  # --- the verdict it names is the verdict the panel renders -----------------------------
  # Written against the MODULE's answer, so it cannot pass by agreeing with itself. This is
  # the check the old gate did not have: the guide used to promise `unknown` to an operator
  # who was about to be shown `destructive`.
  if grep -qiE "\`${VERDICT}\`|${VERDICT}" "$guide" 2>/dev/null; then
    pass
  else
    fail "$label: names the verdict the helper renders today" \
         "module answers '$VERDICT' (source=$VERDICT_SOURCE); the guide never names it"
  fi

  # --- the rollback path is described as the rollback path --------------------------------
  # The dangerous direction: over-promising a down script costs an operator the data written
  # since their backup. When the module says restore, the guide must say restore.
  if [ "$DB_ROLLBACK" = "restore-from-backup" ] && [ "$WITHOUT_DOWN" -gt 0 ]; then
    if grep -qi "restore" "$guide" 2>/dev/null &&
       grep -qiE "data written since|loses everything written since|only for releases whose|verified down script" "$guide" 2>/dev/null; then
      pass
    else
      fail "$label: a restore-from-backup rollback is described as a restore"
    fi
  fi

  # --- it does not claim the reversal gate is unbuilt -------------------------------------
  # Literal, because these were the literal defects. A paraphrase check would miss them and
  # a broader one would fire on §7's honest "not proven yet" section.
  if grep -q "REQ-129 has not landed" "$guide" 2>/dev/null ||
     grep -q "Every plan today reports" "$guide" 2>/dev/null ||
     grep -q "it has not been built" "$guide" 2>/dev/null ||
     grep -q "not built yet" "$guide" 2>/dev/null; then
    fail "$label: no longer claims REQ-129's reversal gate is unbuilt"
  else
    pass
  fi

  # --- both topologies, and the split ----------------------------------------------------
  local want_topologies=0
  for topology in compose kubernetes enterprise; do
    if grep -qi "$topology" "$guide" 2>/dev/null; then want_topologies=$((want_topologies + 1)); fi
  done
  if [ "$want_topologies" -ge 2 ]; then
    pass
  else
    fail "$label: covers both topologies" "only $want_topologies of compose/kubernetes/enterprise present"
  fi
  if grep -qi "application rollback" "$guide" 2>/dev/null &&
     grep -qi "database rollback" "$guide" 2>/dev/null; then
    pass
  else
    fail "$label: splits application from database rollback"
  fi
}

# -------------------------------------------------------------------------------------------
section "the guide exists"
# -------------------------------------------------------------------------------------------
if [ -f "$REAL_GUIDE" ]; then pass; else fail "$REAL_GUIDE exists"; fi

section "the guide's claims match the module's answers"
echo "  tree: $TOTAL_MIGS migrations, $WITH_DOWN with a down script, $WITHOUT_DOWN without"
echo "  helper renders: verdict=$VERDICT source=$VERDICT_SOURCE db_rollback=$DB_ROLLBACK ponr=$PONR"
check_guide "$REAL_GUIDE"

section "the reversal gate the guide names exists"
if [ "$POLICY_EXISTS" = "True" ]; then
  if [ -f "scripts/qa/migration-down-gate.sh" ]; then
    pass
  else
    fail "REQ-129's reversal gate is present" "scripts/qa/migration-down-gate.sh is absent"
  fi
  # And the guide must point at it by path, since the verdict it describes is the gate's.
  if grep -q "migration-down-gate.sh" "$REAL_GUIDE" 2>/dev/null; then
    pass
  else
    fail "the guide names the gate file that produces its verdict"
  fi
else
  fail "REQ-129's policy migration is present" "the guide's verdict claim cannot hold"
fi

# -------------------------------------------------------------------------------------------
section "the guide's order is the plan's order"
# -------------------------------------------------------------------------------------------
# The guide is the judgement; the plan is the order. If they disagree, an operator following
# the prose migrates before backing up. Compared as a SEQUENCE, not as a set: a set cannot
# tell a backup-first plan from a backup-last one, because it holds both the same.
ORDER="$(python3 - <<'PY'
import sys
sys.path.insert(0, "release/lib")
import manifest as m
import upgrade as upg

root = m.repo_root()
shipped = sorted(set(m.discover_migrations(root)))
docs = {
    "from": {"manifest_version": "1", "version": "0.4.0", "registry": "ghcr.io",
             "repository": "raksix/omnion", "core_min": "0.1.0",
             "migrations": shipped[:-3], "migrations_destructive": None},
    "to": {"manifest_version": "1", "version": "0.5.0", "registry": "ghcr.io",
           "repository": "raksix/omnion", "core_min": "0.1.0",
           "migrations": shipped, "migrations_destructive": None},
}
problems = []
for topology in ("compose", "kubernetes"):
    plan = upg.build_plan(from_version="0.4.0", to_manifest=docs["to"],
                          from_manifest=docs["from"], root=root, topology=topology)
    kinds = [s["kind"] for s in plan["steps"]]
    for earlier, later in (("backup", "migrate"), ("migrate", "deploy")):
        if earlier in kinds and later in kinds and kinds.index(earlier) > kinds.index(later):
            problems.append(f"{topology}: {earlier} after {later}")
    # A deploy before the migration is the failure the whole guide is built to prevent.
    if kinds[:1] == ["deploy"]:
        problems.append(f"{topology}: deploy is the first step")

# The prose must present the same order, and it numbers the compose steps.
text = open("docs/deployment/upgrade.md", encoding="utf-8").read()
def step(n):
    marker = f"# {n}. "
    candidates = [text.find(marker, i) for i in range(len(text))]
    candidates = [c for c in candidates if c != -1]
    return min(candidates) if candidates else -1
positions = [step(n) for n in (1, 2, 3, 4)]
if all(p != -1 for p in positions) and positions != sorted(positions):
    problems.append("the guide's numbered steps are not in ascending document order")

print("; ".join(problems))
PY
)"
if [ -z "$ORDER" ]; then
  pass
else
  fail "backup precedes migrate precedes deploy, in the plan and in the prose" "$ORDER"
fi

# -------------------------------------------------------------------------------------------
section "mutations — each check must be able to fail"
# -------------------------------------------------------------------------------------------
# Each mutation writes a broken COPY and asserts this gate reports a failure on it. The real
# guide is never touched, so a failed mutation leaves the repository exactly as it was.
MUTDIR="$WORK/mut"
mkdir -p "$MUTDIR"

mutate_guide() { # <name> <python-statement-list>
  local name="$1" recipe="$2" out="$MUTDIR/$1.md"
  cp "$REAL_GUIDE" "$out"
  if ! python3 - "$out" "$recipe" <<'PY'
import re, sys
path, recipe = sys.argv[1], sys.argv[2]
original = open(path, encoding="utf-8").read()
# The recipe mutates `text` inside a NAMESPACE, and the namespace is read back afterwards.
#
# `exec(code, {"text": text})` does NOT mutate the enclosing local — the dict is the module's
# global mapping, so the assignment lands in the dict and the local keeps its old value. The
# first version of this file did exactly that, so every mutation below rewrote an IDENTICAL
# copy of the guide, ran a perfectly valid check against it, correctly found nothing wrong,
# and reported six green mutations that had broken nothing at all. This is the same
# silent-no-op class `release-upgrade.sh` documents for a `str.replace` whose target did not
# exist — which is why the assertion that the copy actually CHANGED is below and not optional.
ns = {"text": original, "re": re}
exec(recipe, ns)
mutated = ns["text"]
if mutated == original:
    sys.stderr.write("mutation produced an IDENTICAL document — the recipe is a no-op\n")
    sys.exit(2)
open(path, "w", encoding="utf-8").write(mutated)
PY
  then
    fail "MUTATION: $name — the recipe did not change the document"
    printf '%s' "$out"
    return
  fi
  printf '%s' "$out"
}

expect_red() { # <name> <expected-fragment-in-the-failure> <path>
  local name="$1" fragment="$2" guide="$3" before_fail="$FAIL"
  # NOTE: `check_guide` is called DIRECTLY with its output redirected to a file, never
  # through `$( )`. Command substitution runs the function in a SUBSHELL, so its `fail`
  # calls would increment a counter that dies with the subshell and the parent would always
  # see "no new failure" — every mutation would report caught-by-accident. The first version
  # of this file made exactly that mistake and reported six green mutations that had measured
  # nothing at all. A redirection does not fork.
  check_guide "$guide" > "$WORK/mutation-output.txt" 2>&1
  if [ "$FAIL" -gt "$before_fail" ]; then
    if grep -qF "$fragment" "$WORK/mutation-output.txt"; then
      pass
      printf '  mutation caught: %s\n' "$name"
    else
      fail "MUTATION: $name went red, but not for the expected reason" \
           "expected: $fragment; got: $(grep -m1 'FAIL:' "$WORK/mutation-output.txt")"
    fi
  else
    fail "MUTATION: $name — the gate stayed green on a broken document"
  fi
  FAIL="$before_fail"
}

# 1. The stale claim. This is the actual defect this gate was written for: the guide told an
#    operator to expect `unknown` while the panel renders `destructive`.
M1="$(mutate_guide stale \
  'text = text.replace("A migration that ships no down script is", "REQ-129 has not landed. A migration file that ships no down script is")')"
expect_red "the stale-claim guard" "REQ-129's reversal gate is unbuilt" "$M1"

# 2. A wrong census. The numbers are read from disk, so the guide cannot quote a stale count.
#    Every occurrence is rewritten, not just the first: this mutation caught the fact that a
#    `count=1` replacement leaves the guide's SECOND statement of the number intact, so the
#    check found the untouched sentence and passed. Two sentences carrying one number is
#    normal in prose; a gate that only rewrites one of them is measuring the mutation.
M2="$(mutate_guide census \
  'text = re.sub(r"[0-9]+ of [0-9]+ migrations", "12 of 99 migrations", text)
text = text.replace("61 migrations ship, and **54 of them carry no down", "99 migrations ship, and **12 of them carry no down")')"
expect_red "the migration census" "migration census matches the tree" "$M2"

# 3. Every command deleted. The four word greps this gate replaces stayed green through this;
#    the command count is what catches it.
M3="$(mutate_guide nocommands \
  'text = re.sub(r"```bash\n.*?```", "", text, flags=re.S)')"
expect_red "the runnable-command count" "runnable commands for both topologies" "$M3"

# 4. A command that does not parse — the shape an operator pastes into a production shell.
#    Two lessons in one mutation, both found by running it:
#      * a trailing `&&` on its OWN LINE is valid bash (`bash -n` returns 0), so the first
#        version of this mutation passed. The unparseable shape is an unclosed quote or an
#        unclosed command substitution, which is what a truncated paste actually produces.
#      * the migrate command is the SECOND line of the first block, so replacing the phrase
#        with `count=1` never reached it — the block simply has no such line to change.
M4="$(mutate_guide badcommand \
  'text = text.replace("docker compose -f docker-compose.prod.yml run --rm migrate", "docker compose -f docker-compose.prod.yml run --rm \x27migrate", 1)')"
expect_red "bash -n over the guide's own commands" "every bash command it shows parses" "$M4"

# 5. The rollback split removed: an operator told a database rollback is always available.
M5="$(mutate_guide norollback \
  'text = text.replace("Database rollback", "Rollback").replace("database rollback", "rollback")')"
expect_red "the rollback split" "splits application from database rollback" "$M5"

# 6. The verdict names gone. Every word the module uses for its three verdicts is rewritten,
#    so the guide no longer tells an operator which one the panel renders — the drift this
#    gate exists for. The census and the commands survive, so the only check that can catch
#    this one is the verdict check; that is the point.
M6="$(mutate_guide noverdict \
  'text = re.sub(r"destructive|unknown|reversible", "a state", text)')"
expect_red "the verdict the helper renders" "names the verdict the helper renders today" "$M6"

# -------------------------------------------------------------------------------------------
section "summary"
echo "upgrade guide gate: ${PASS} passed, ${FAIL} failed"
if [ "$FAIL" -gt 0 ]; then
  printf '  failed: %s\n' "${FAILED_NAMES[@]}"
  exit 1
fi
exit 0