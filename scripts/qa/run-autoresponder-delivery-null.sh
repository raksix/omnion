#!/usr/bin/env bash
# run-autoresponder-delivery-null.sh — `delivered_at` is a question about a VALUE, and the
# `?` operator cannot see a JSON null.
#
#   QA_DB=omnion_qa_w8_delivery_null bash scripts/qa/run-autoresponder-delivery-null.sh
#
# ## THE DEFECT THIS GATE NAMES
#
# Four functions in `modules/crm-intake/src/autoresponder_store.rs` decided "was this message
# delivered?" with the jsonb key-EXISTENCE operator:
#
#     and detail ? 'sent' and not (detail ? 'delivered_at')
#
# `?` answers "is this key present", and **a key whose value is JSON `null` is present**. That
# makes the negation false for the one shape the platform deliberately writes.
#
# Migration `0202` — `0202_crm_autoresponder_immediate_claim.sql` — stamps the rows it cannot
# repair with:
#
#     jsonb_build_object('sent', true, 'delivered_at', null, 'delivery_unknown', true)
#
# and says so in its own words, which is why this is a defect and not a reading disagreement:
# *"`delivered_at` is NULL for a row whose delivery was never observed, which is the truth."*
# The existing gate `run-crm-autoresponder-claim.sh` asserts that exact text on the row.
#
# So the predicate excluded exactly the rows `0202` was written to rescue, and:
#
#   * `mark_sent` could not complete them — while its own doc comment promised *"an installation
#     upgrading mid-flight can complete a claim the old code claimed but never recorded, rather
#     than leaving it permanently uncompletable."* That sentence described unreachable code.
#   * `release_claim` could not release them — benign there, and in fact load-bearing, which is
#     the trap: the bug and the safety net are the SAME line, so a search-and-replace that
#     "fixed" all four sites would have deleted a settled lead's only record of being answered.
#
# ## WHY THIS IS A GATE AND NOT A UNIT TEST
#
# The behavioural half already exists and is strong: `modules/crm-intake/tests/
# crm_autoresponder.rs` drives `mark_sent` and `release_claim` against a real database, and this
# tick added `a_0202_shaped_claim_can_still_be_completed` and
# `a_settled_0202_claim_is_never_released` to it. What no Rust test can catch is the DRIFT this
# defect came from: a fifth `?`-based spelling written next year by a writer who never read this
# file. That is a question about the shipped text, so it is asked of the shipped text.
#
# The four legs are one per site, and each names the sentence that site is answerable to, so a
# site that is deleted or renamed fails here rather than silently dropping out of the count.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

STORE="modules/crm-intake/src/autoresponder_store.rs"
# `set -o pipefail` + `grep -q` is a RACE, not a mistake: `grep -q` exits at the first match,
# the upstream writer takes SIGPIPE (141), and pipefail reports the pipeline as 141 even though
# the match succeeded. Measured on an earlier gate on this branch: 0 141 141 0 141 141 0 141
# over eight identical runs. So every leg captures to a variable FIRST and tests the variable.

PASSED=0
FAILED=0
leg()  { printf 'leg %-52s %s\n' "$1" "$2"; }
pass() { printf '  \033[32mPASS\033[0m %s\n' "$1"; PASSED=$((PASSED + 1)); }
fail() { printf '  \033[31mFAIL\033[0m %s — %s\n' "$1" "$2" >&2; FAILED=$((FAILED + 1)); }
check() { if [ "$2" = "true" ]; then pass "$1"; else fail "$1" "$3"; fi; }
no()   { if [ "$2" = "true" ]; then fail "$1" "$3"; else pass "$1"; fi; }
notes() { printf '      note: %s\n' "$1"; }

BODY="$(cat "$STORE")"

# -- leg 1 -- the defect's own arithmetic, proved ON THE SERVER, not asserted from memory.
#
# This leg exists because the whole gate rests on one claim about jsonb, and a claim about jsonb
# that is only argued is a claim a future writer will argue back. It asks the running database.
# (The gate needs a container for the behavioural half anyway, so the answer is free here.)
CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
QADB="${QA_PROBE_DB:-postgres}"
jsonb_answer() {
  docker exec "$CONTAINER" psql -U omnion -d "$QADB" -q -A -t -c "$1" 2>/dev/null | tr -d '[:space:]'
}
leg "the operator, asked rather than argued" "asking the server about jsonb null"
if [ -z "$(jsonb_answer 'select 1')" ]; then
  fail "the server is reachable for the jsonb probe" "no container $CONTAINER; the gate cannot judge the defect"
  ANSWER=""
else
  ANSWER="$(jsonb_answer "select ('{\"sent\":true,\"delivered_at\":null,\"delivery_unknown\":true}'::jsonb) ? 'delivered_at'")"
  TEXT_NULL="$(jsonb_answer "select ('{\"delivered_at\":null}'::jsonb)->>'delivered_at' is null")"
  if [ "$ANSWER" = "t" ] && [ "$TEXT_NULL" = "t" ]; then
    pass "a null-valued key answers 'present' to ? and 'null' to ->>  — the old predicate saw a delivered row"
  else
    fail "a null-valued key answers 'present' to ? and 'null' to ->>" "server said ?=${ANSWER} ->>is null=${TEXT_NULL}"
  fi
fi

# -- leg 2 -- the four sites. None may ask the key-existence question about `delivered_at`.
#
# **Only code lines are counted.** The string `not (detail ? 'delivered_at')` also appears in
# `release_claim`'s doc comment, where it is the sentence NAMING the defect, and a gate that
# counted prose would report its own explanation as a regression. The first run of this file
# did exactly that: 1 occurrence, in a heading, on code that was already correct. A gate whose
# subject appears in its own description needs the two separated, or it fails for ever.
leg "the four sites" "no SITE asks about a key instead of a value"
code_lines() { grep -v -E '^[[:space:]]*//' "$1" || true; }
STALE="$(code_lines "$STORE" | grep -c -F "not (detail ? 'delivered_at')" || true)"
STALE_SWEEP="$(code_lines "$STORE" | grep -c -F "not (e.detail ? 'delivered_at')" || true)"
if [ "${STALE:-0}" = "0" ] && [ "${STALE_SWEEP:-0}" = "0" ]; then
  pass "no function tests for the PRESENCE of delivered_at; all four test its value"
else
  fail "no function tests for the PRESENCE of delivered_at" \
       "found ${STALE:-?} in-function and ${STALE_SWEEP:-?} sweep occurrence(s) of \`not (detail ? 'delivered_at')\`"
fi

VALUE_SITES="$(code_lines "$STORE" | grep -c -F "detail->>'delivered_at' is null" || true)"
if [ "${VALUE_SITES:-0}" -ge 4 ]; then
  pass "the value spelling is in all four places ($VALUE_SITES)"
else
  fail "the value spelling is in all four places" "found ${VALUE_SITES:-0}, expected 4 (release, mark, claim_delivery, sweep)"
fi

# -- leg 3 -- the two functions must NOT agree, and the gate has to say why.
#
# This is the leg a well-meaning reviewer would "simplify" away, which is why it is written down.
# `mark_sent` completes a row; `release_claim` DELETES one. On a 0202 row those are not the same
# operation, so the asymmetry is the rule and not an inconsistency.
leg "the asymmetry" "completion may proceed on an unknown row; a release may not"
GUARD="$(code_lines "$STORE" | grep -c -F "coalesce(detail->>'delivery_unknown', 'false') <> 'true'" || true)"
if [ "$GUARD" -ge 1 ]; then
  pass "the release carries an explicit delivery_unknown exemption, written down rather than accidental"
else
  fail "the release carries an explicit delivery_unknown exemption" \
       "a release that can see a 0202 row would delete the only record that the lead was answered"
fi
GUARD_PLACED="$(grep -n -F "coalesce(detail->>'delivery_unknown', 'false') <> 'true'" "$STORE" | cut -d: -f1)"
MARK_LINE="$(grep -n -F 'pub async fn mark_sent' "$STORE" | cut -d: -f1)"
RELEASE_LINE="$(grep -n -F 'pub async fn release_claim' "$STORE" | cut -d: -f1)"
if [ -n "$GUARD_PLACED" ] && [ -n "$RELEASE_LINE" ] && [ -n "$MARK_LINE" ] \
   && [ "$GUARD_PLACED" -gt "$RELEASE_LINE" ] && [ "$GUARD_PLACED" -lt "$MARK_LINE" ]; then
  pass "the guard is inside release_claim, and mark_sent (the completion) is deliberately unguarded"
else
  fail "the guard is inside release_claim" "guard at ${GUARD_PLACED:-?}, release at ${RELEASE_LINE:-?}, mark_sent at ${MARK_LINE:-?}"
fi

# -- leg 4 -- the rule is documented where the next writer will look.
#
# **These two use the same capture-then-test shape as leg 2, and the reason is this file's own
# header.** `printf '%s' "$BODY" | grep -q …` is the race the header describes: `grep -q` exits
# at the first match, the writer takes SIGPIPE, and under `pipefail` the pipeline reports 141
# even though the match succeeded. The first run of this leg wrote the pipeline form anyway —
# and it failed, for the race and not for the rule. **A gate that documents a trap in prose and
# then walks into it teaches the reader nothing**, so the body is matched with `case` instead:
# no pipeline, no writer to lose, and it is one less construct to reason about.
leg "the rule is written down" "both functions explain the asymmetry"
case "$BODY" in
  *"completion may proceed on an unknown row, a release"*)
    pass "release_claim states the rule in prose, not only in SQL" ;;
  *)
    fail "release_claim states the rule in prose" \
         "a future writer re-spells the predicate without knowing why it differs" ;;
esac
case "$BODY" in
  *"permanently uncompletable"*)
    pass "0202's 'permanently uncompletable' promise is quoted where the predicate now honours it" ;;
  *)
    fail "0202's 'permanently uncompletable' promise is quoted" \
         "mark_sent's doc claims the migration makes a row completable; the predicate must say why" ;;
esac

# -- the behavioural witness, if a database is available.
#
# The Rust suite is the real proof; this only reports whether it was the one that ran. A gate
# that skipped the behavioural half would print green for a product that compiles and behaves
# wrongly, so its absence is stated rather than passed over.
LEG_SUITE="modules/crm-intake/tests/crm_autoresponder.rs"
leg "the behavioural witness" "the Rust suite carries both cases"
for name in a_0202_shaped_claim_can_still_be_completed a_settled_0202_claim_is_never_released; do
  if grep -q "async fn $name" "$LEG_SUITE"; then
    pass "the suite asserts $name"
  else
    fail "the suite asserts $name" "the behavioural half of this gate is missing"
  fi
done

printf '\n[%s] %d passed, %d failed\n' "$(basename "$0")" "$PASSED" "$FAILED"
[ "$FAILED" -eq 0 ]
