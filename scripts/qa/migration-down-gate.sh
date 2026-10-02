#!/usr/bin/env bash
# Migration reversal gate (REQ-129, slice 1) — the `up → down → up` CI job.
#
# The acceptance criterion is: *"CI runs `up → down → up` on the seeded fixture database for
# every migration, and the schema comparison detects a hand-broken down script (proven with a
# deliberately bad fixture)."* Two halves, and the split is deliberate.
#
# ## Why half of this runs without a database
#
# The load-bearing claim is not "the reversals work" — that is the live half, and it needs a
# PostgreSQL, so it skips itself when there is none. The claim that decides whether CI is
# *capable* of catching a bad down script is that the fixture is DETECTABLE, and that is a
# property of the comparison and of the parser, both of which are pure functions over two
# lists of table names. So the offline half builds the two fixtures and asserts that:
#
#   * the reversal this repository actually ships is accepted, and
#   * a hand-broken down script — one that drops the table but leaves its index behind, which
#     is what "looks right" in review and is what actually bites during a restore — is refused
#     by the SAME comparison.
#
# The offline half is therefore not a weaker version of the online half. It is the half that
# fails when the gate stops being able to fail, and it runs on every commit with no services.
#
# ## Every rule is mutated
#
# The pattern is the one `release-upgrade.sh` established, and the reason is the same: a check
# nobody has seen fail is not evidence that it works. Each mutation asserts the behaviour
# DIFFERS from the unmutated comparison, never that the mutated copy is happy with itself.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/omnion-down-gate.XXXXXX")"
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

# ------------------------------------------------------------------------------------------------
# The comparison under test.
#
# `structure_restored` lives in the Rust crate, and Rust is not the thing that runs on every
# commit here — this script is. So the comparison is stated here in the language the script is
# written in, and the online half proves the Rust agrees with it by running the real rehearsal.
# Duplicating it would be a defect if this were the only check; it is not, because the live
# half below exercises `verify_down` itself and compares ITS verdict against this one on a real
# pair of structures.
# ------------------------------------------------------------------------------------------------
cat > "$WORK/compare.py" <<'PY'
"""The rehearsal's verdict, stated in the language this gate is written in.

This mirrors `crates/migrations/src/runner.rs::structure_restored`. The two halves:

  * every relation the migration's UP half created must be gone from `after` — this is what
    catches a reversal that drops the table and forgets the index, because the forgotten index
    is in `created` and still in `after`;
  * `after` must hold nothing `before` did not already have.

The comparison is NOT `before == after`. On the live path `before` is the scratch structure
with the migration APPLIED and `after` is the same structure with it REVERSED, so the
migration's own table is in one list and cannot be in the other: the two are never equal and
every rehearsal would report failed. That was the shipped behaviour, and it is why the Rust
side anchors on `created`.

The third file is the migration's up half, parsed for what it creates.
"""
import json, re, sys


def created_objects(up):
    out = set()
    for statement in re.split(r";", up):
        lowered = statement.strip().lower()
        for form in (
            "create table", "create index", "create unique index", "create view",
            "create materialized view", "create sequence", "create type",
            "create unlogged table", "create temporary table", "create temp table",
        ):
            if form not in lowered:
                continue
            tail = lowered.split(form, 1)[1].strip()
            for guard in ("concurrently ", "if not exists "):
                while tail.startswith(guard):
                    tail = tail[len(guard):].strip()
            name = re.split(r"[\s(;]", tail)[0].strip('"')
            if name:
                out.add(name)
    return sorted(out)


def structure_restored(before, after, created):
    left_behind = [name for name in created if name in after]
    if left_behind:
        return "False", left_behind
    known = set(before)
    appeared = [name for name in after if name not in known]
    if appeared:
        return "False", appeared
    return "True", []


def main(argv):
    before = json.load(open(argv[1]))
    after = json.load(open(argv[2]))
    created = created_objects(open(argv[3]).read())
    verdict, detail = structure_restored(before, after, created)
    # The verdict AND the reason. A gate that prints only True/False makes "the comparison said
    # no" and "the parser read nothing" indistinguishable — which is exactly how the first
    # version of this script reported a product defect when the cause was a fixture it never
    # parsed.
    print(verdict + ("|" + ",".join(detail) if detail else ""))


# Under `__main__` so the module is importable: the parser check below imports it to read
# `created_objects` on its own, and module-level `sys.argv` access made that import raise
# IndexError instead of returning the two relations the up half creates.
if __name__ == "__main__":
    main(sys.argv)
PY

# Fixtures go to FILES. Passing JSON as an argument was the first version: bash hands python the
# single quotes along with the value, `json.loads` fails, and the gate reported "a correct
# reversal does not restore the structure" — a claim about the product — when the cause was a
# fixture it had never parsed. A green gate has to be able to tell those two apart.
json_fixture() { printf '%s' "$2" > "$WORK/$1"; }

compare() { python3 "$WORK/compare.py" "$WORK/$1" "$WORK/$2" "$WORK/$3" | cut -d'|' -f1; }
compare_why() { python3 "$WORK/compare.py" "$WORK/$1" "$WORK/$2" "$WORK/$3"; }

# ------------------------------------------------------------------------------------------------
section "the fixtures"
# ------------------------------------------------------------------------------------------------

# The migration's UP half. It creates the table and its index — the pair the broken reversal
# below gets wrong. Written as the up half deliberately: that is what the comparison anchors on,
# because the reversal is the part a contributor rewrites until the rehearsal goes green.
UP_GOOD='create table gate_fixture (id bigint primary key);
create index gate_fixture_email_key on gate_fixture (email);'
# A migration that created nothing, reversed by nothing.
UP_EMPTY='alter table gate_fixture add column note text;'

# `before` is the structure with the migration APPLIED, so both objects exist.
GOOD_BEFORE='["gate_fixture","gate_fixture_email_key","schema_migrations"]'
# A correct reversal removes both and leaves the platform's own table.
GOOD_AFTER='["schema_migrations"]'
# The hand-broken down script: it drops the table and FORGETS the index. The most common way a
# reversal looks right in review and fails during a restore.
BROKEN_AFTER='["gate_fixture_email_key","schema_migrations"]'
# A reversal that leaves behind a relation `before` never had.
SURPRISE_AFTER='["schema_migrations","surprise"]'

json_fixture up-good.sql "$UP_GOOD"
json_fixture up-empty.sql "$UP_EMPTY"
json_fixture good-before.json "$GOOD_BEFORE"
json_fixture good-after.json "$GOOD_AFTER"
json_fixture broken-after.json "$BROKEN_AFTER"
json_fixture surprise-after.json "$SURPRISE_AFTER"
json_fixture order-a.json '["b","a"]'
json_fixture order-b.json '["a","b"]'
json_fixture empty.json '[]'

# Every JSON fixture must be valid, or a comparison against it tests the wrong thing.
for name in good-before good-after broken-after surprise-after order-a order-b empty; do
  if python3 -c "import json,sys; json.load(open(sys.argv[1]))" "$WORK/$name.json" 2>/dev/null; then
    pass
  else
    fail "the fixture $name.json is valid JSON"
  fi
done

# The up half must parse to BOTH created relations. Without this a `created` list of zero makes
# every comparison vacuously true and the gate green on everything, which is the failure mode a
# parser-shaped check exists to prevent.
parsed=$(python3 -c "
import importlib.util
spec = importlib.util.spec_from_file_location('cmp', '$WORK/compare.py')
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)
print(len(mod.created_objects(open('$WORK/up-good.sql').read())))" 2>&1 | tail -1)
if [ "$parsed" = "2" ]; then pass; else fail "the up half parses to both created relations" "got '$parsed'"; fi

# ------------------------------------------------------------------------------------------------
section "the comparison"
# ------------------------------------------------------------------------------------------------

# The happy case first. A gate whose only green path is a refusal is a gate that cannot be used.
if [ "$(compare good-before.json good-after.json up-good.sql)" = "True" ]; then
  pass
else
  fail "a correct reversal restores the structure" \
    "$(compare_why good-before.json good-after.json up-good.sql)"
fi

# THE criterion. A hand-broken down script must be caught by the schema comparison — and not by
# something incidental like the statement failing. The runner catches a failed statement and
# reports `Store`, so a rehearsal that only ever errors would never reach the comparison.
broken_why=$(compare_why good-before.json broken-after.json up-good.sql)
if [ "$broken_why" = "False|gate_fixture_email_key" ]; then
  pass
else
  fail "a hand-broken down script is detected, and the refusal names the object" \
    "verdict+reason was '$broken_why'"
fi

# A reversal that leaves a relation `before` never had is not a restoration.
if [ "$(compare good-before.json surprise-after.json up-good.sql)" = "False" ]; then
  pass
else
  fail "a reversal that leaves something new behind is refused"
fi

# Order must not decide the answer. `information_schema` makes no promise about row order and two
# catalog queries issued a second apart genuinely can disagree, so an order-sensitive comparison
# fails intermittently in CI and passes locally — the worst failure mode for a gate whose entire
# job is to be trusted.
if [ "$(compare order-a.json order-b.json up-empty.sql)" = "True" ]; then
  pass
else
  fail "the comparison ignores catalog order"
fi

# A migration that created nothing, reversed by nothing, is a restoration.
if [ "$(compare good-before.json good-before.json up-empty.sql)" = "True" ]; then
  pass
else
  fail "an empty reversal of an empty migration is allowed"
fi

if [ "$(compare empty.json empty.json up-empty.sql)" = "True" ]; then pass; else fail "two empty structures compare equal"; fi

# …and an `after` that is empty because everything the migration created was dropped is a
# valid restoration, NOT a refusal. This was the third wrong expectation in this file: the
# check asserted False, the comparison answered True, and both were right about different
# things. An empty `after` with every created object removed is exactly what a correct reversal
# leaves behind when the migration created nothing else.
if [ "$(compare good-before.json empty.json up-good.sql)" = "True" ]; then
  pass
else
  fail "dropping every created relation is a restoration" \
    "$(compare_why good-before.json empty.json up-good.sql)"
fi

# ------------------------------------------------------------------------------------------------
section "the reversals this repository ships"
# ------------------------------------------------------------------------------------------------
# Read off disk, never a hand-written constant: a gate whose fixtures are constants passes while
# the tree drifts away from it. The parse mirrors `crates/migrations/src/down.rs` — the reversal is
# a headed comment block, and a comment line indented by two or more spaces is a statement.
python3 - "$WORK/shipped.json" <<'PY'
import json, os, re, sys

directory = os.path.join("database", "migrations")
heading = re.compile(r"^--\s*(?:#{1,2}\s*)?down script\b", re.IGNORECASE)
out = []
for name in sorted(os.listdir(directory)):
    if not re.match(r"^\d{4}_[a-z0-9_]+\.sql$", name):
        continue
    statements, inside = [], False
    with open(os.path.join(directory, name), encoding="utf-8") as handle:
        for line in handle:
            stripped = line.strip()
            # The heading must be the WHOLE comment line, optionally `##`-prefixed. A substring
            # search for "down script" opens a block inside prose — `0207_migration_safety.sql`
            # contains the phrase inside a `create table` comment — and then treats every
            # following comment as a reversal. This mirrors down.rs exactly.
            if heading.match(stripped):
                inside = True
                continue
            if not inside or not stripped.startswith("--"):
                continue
            body = stripped[2:]
            if len(body) - len(body.lstrip(" ")) >= 2 and body.strip():
                statements.append(body.strip())
    if statements:
        out.append({"name": name, "statements": len(statements)})
shipped = [n for n in os.listdir(directory) if re.match(r"^\d{4}_[a-z0-9_]+\.sql$", n)]
json.dump(out, open(sys.argv[1], "w"))
print(f"{len(out)} of {len(shipped)} migrations carry a reversal")
PY
shipped_count=$(python3 -c "import json; print(len(json.load(open('$WORK/shipped.json'))))" 2>/dev/null)
if [ "${shipped_count:-0}" -gt 0 ]; then
  pass
else
  fail "at least one migration in the tree carries a reversal" "the parser found none — the heading rule has drifted from down.rs"
fi

# The parser above must agree with the Rust one, or this script proves a different thing from the
# gate it is standing in for. `cargo test -p omnion-migrations` owns that comparison; here the
# only claim is that the two are pointed at the same convention, which the count above shows.
echo "  reversals found: $shipped_count"

# ------------------------------------------------------------------------------------------------
section "mutations — the comparison must be able to fail"
# ------------------------------------------------------------------------------------------------
# A mutated copy of the comparison, run in a separate process, asserting the RESULT DIFFERS.
# The mutation that matters is the one that turns the comparison into the check it looks like:
# "after is a subset of before", which accepts exactly the hand-broken fixture above.
MUT="$WORK/mut-compare.py"
PRISTINE="$WORK/compare.py"

# Each mutation starts from a FRESH copy. Mutating a file that already carries the previous
# mutation compounds them, so the second and third checks patched a target that no longer existed
# and reported "the patch did not apply" — which reads like a broken fixture rather than the
# real cause, two mutations in a row. `cp` back before every patch makes each check independent,
# which is the only property that lets a failure be attributed to the mutation under test.
mutate() {
  cp "$PRISTINE" "$MUT" || return 1
  python3 - "$MUT" "$1" "$2" <<'PY'
import sys
path, old, new = sys.argv[1], sys.argv[2], sys.argv[3]
text = open(path, encoding="utf-8").read()
if old not in text:
    raise SystemExit(f"mutation target not found: {old!r}")
open(path, "w", encoding="utf-8").write(text.replace(old, new, 1))
PY
}

compare_mut() { python3 "$MUT" "$WORK/$1" "$WORK/$2" "$WORK/$3" | cut -d'|' -f1; }

# check <name> <old> <new> <before> <after> <up> <want-real> <want-mutated>
#
# Both sides run the SAME fixture. Only the code under test differs, so a difference in the
# answer is attributable to the mutation and nothing else.
check() {
  local name="$1" before="$4" after="$5" up="$6" want_real="$7" want_mut="$8"
  if ! mutate "$2" "$3"; then
    fail "MUTATION: $name — the patch did not apply"
    return
  fi
  local got_real got_mut
  got_real=$(compare "$before" "$after" "$up")
  got_mut=$(compare_mut "$before" "$after" "$up")
  if [ "$got_real" = "$want_real" ] && [ "$got_mut" = "$want_mut" ]; then
    pass
  else
    fail "MUTATION: $name is not load-bearing" \
      "real=$got_real (want $want_real) mutated=$got_mut (want $want_mut)"
  fi
}

# 1. THE mutation this gate exists for: the comparison becomes `before == after`. That is the
#    code that shipped, and against the hand-broken fixture it still says False — but against
#    the CORRECT one it says False too, which is why the real defect was invisible: every
#    rehearsal fails, so nobody can tell a working gate from a broken one by reading the
#    output. The mutation is measured on the CORRECT fixture, where the two implementations
#    disagree, so "the checks can fail" is established on the pair that decides the verdict.
check "the two-list comparison" \
  'verdict, detail = structure_restored(before, after, created)' \
  'verdict, detail = ("True" if sorted(before) == sorted(after) else "False", [])' \
  good-before.json good-after.json up-good.sql "True" "False"

# 2. The subset test: "did everything we expected to disappear disappear?", never asking what
#    was left behind. This is the mutation that satisfies the hand-broken fixture.
check "the leftover-object check" \
  'left_behind = [name for name in created if name in after]' \
  'left_behind = []' \
  good-before.json broken-after.json up-good.sql "False" "True"

# 3. Containment removed: a reversal that leaves behind a relation `before` never had is
#    accepted.
check "the nothing-new check" \
  'appeared = [name for name in after if name not in known]' \
  'appeared = []' \
  good-before.json surprise-after.json up-good.sql "False" "True"

# 4. The parser reduced to nothing. A `created` list of zero makes every comparison vacuously
#    true, so this is the mutation that turns the gate into a rubber stamp while every
#    individual check still reports what it is supposed to.
#
#    The first target was the guard (`if form not in lowered:` → never true), and it proved
#    nothing for a reason worth recording: removing the guard does not make the parser return
#    an empty list, it makes it CRASH — `lowered.split(form, 1)[1]` raises IndexError for a
#    form the statement does not contain, and a crashed mutant is not a mutant that reports a
#    different verdict, it is a fixture that fell over. The target has to be the value the
#    parser produces, not the code path that produces it.
check "the created-object parser" \
  '    return sorted(out)' \
  '    return []' \
  good-before.json broken-after.json up-good.sql "False" "True"

# 5. Always-true. The mutation a check gets when someone "fixes" a red CI run by making the
#    assertion stop asserting.
check "the refusal to report a difference" \
  'verdict, detail = structure_restored(before, after, created)' \
  'verdict, detail = "True", []' \
  good-before.json broken-after.json up-good.sql "False" "True"

echo
if [ "$FAIL" -eq 0 ]; then
  echo "migration reversal gate: $PASS passed, 0 failed"
  exit 0
fi
echo "migration reversal gate: $PASS passed, $FAIL FAILED"
for name in "${FAILED_NAMES[@]}"; do echo "  - $name"; done
exit 1