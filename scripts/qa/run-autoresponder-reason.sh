#!/usr/bin/env bash
# run-autoresponder-reason.sh — ONE trail line must not name its verdict two ways, and only one
# of the two may be the word "sent".
#
# THE DEFECT THIS GATE NAMES
#
# Slice 45 removed the three copies of "may this go to the mailer now" and made
# `Delivery::sendable()` the single owner. Slice 46 named the five states a *stored claim* can
# be in (`ClaimState`) and put that state on the lead timeline. What was left behind is the
# other half of the same sentence, still in its own spelling:
#
#     pub fn reason(&self) -> &'static str {
#         match self {
#             Self::Ready(_) => "sent",          // <-- a DELAYED Ready lands here too
#             ...
#         }
#     }
#
# `Ready` does not mean "handed to the mailer". `Autoresponder::deliver()` returns
# `Ready(Message { delayed: true, .. })` for any source with a non-zero `delay_minutes`
# (autoresponder.rs:248) — which is the *reservation* mechanism, not a send. So a 30-minute
# autoresponder writes a trail line whose `reason` is the word `sent`, next to a
# `ClaimState::Reserved` chip the panel now renders from the same row. **One line, two
# verdicts, and the false one is the word an operator scans for.**
#
# The route compounds it. `crm_intake.rs` calls `record_skip(... outcome.verdict.reason() ...)`
# in the arm that runs when `sendable()` is `None` — the arm whose own comment says "A delayed
# message is the worker's to send … Both leave a line so the detail page can say which." The
# comment is the specification and the argument is its violation: a delayed claim reaches
# `Ready`, `sendable()` is `None`, the comment's promise fires, and the reason written is
# `sent`. **A comment that says which two cases a branch handles, next to a branch that
# handles them with the same value, is the shape this branch keeps meeting.**
#
# There is a second, quieter half. `Delivery::NotYet(OffsetDateTime)` is the variant that
# *should* own the word "delayed" — and it is constructed NOWHERE in the repository outside its
# own test module. `deliver()` never returns it, because a delayed source returns
# `Ready { delayed: true }` instead. So the one variant whose name is the answer has no
# producer, while the one variant that has a producer answers with a different word. **A
# variant with no producer is not dead code to be tidied away; it is evidence that the rule was
# never stated, and here it is the only thing that makes the defect visible at all.**
#
# WHY A PURE-FUNCTION GATE IS THE RIGHT INSTRUMENT
#
# The disagreement is between a `match` arm, a route argument and a panel render — three
# places, none of which is a runtime behaviour. A browser pass would measure the panel's
# pixels and not the reason string, and a `cargo test` that calls `reason()` proves `reason()`
# agrees with itself, which is the tick-68 shape. These read the shipped files, so a later
# writer who re-spells the rule is caught here rather than in a customer's inbox.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

AUTORESPONDER="modules/crm-intake/src/autoresponder.rs"
ROUTE="apps/api/src/routes/crm_intake.rs"
STORE="modules/crm-intake/src/autoresponder_store.rs"
PANEL="apps/admin/features/crm-intake/lead-detail.tsx"
LIB="apps/admin/lib/crm-intake.ts"

leg()  { printf 'leg %-46s %s\n' "$1" "$2"; }
pass() { printf '  \033[32mPASS\033[0m %s\n' "$1"; PASSED=$((PASSED + 1)); }
fail() { printf '  \033[31mFAIL\033[0m %s — %s\n' "$1" "$2"; FAILED=$((FAILED + 1)); }
no()   { if [ "$2" = "true" ]; then fail "$1" "$3"; else pass "$1"; fi; }
check() { if [ "$2" = "true" ]; then pass "$1"; else fail "$1" "$3"; fi; }
notes() { printf '      note: %s\n' "$1"; }

# `has PATTERN TEXT` — is PATTERN (an extended regex) present in TEXT?
#
# ## Why this helper exists rather than `printf '%s' "$X" | grep -qE "$P"`
#
# The first version of this gate used the pipeline form on every leg, and **four of its seven
# positive checks read `false` against code that was present**. The cause is a race, not a
# mistake of the pattern:
#
#     printf '%s' "$CODE" | grep -qE 'pattern'      # under `set -o pipefail`
#
# `grep -q` exits at the FIRST match, so the upstream `printf`/`grep` is still writing when
# the reader goes away and dies of SIGPIPE (141). `pipefail` then reports the pipeline's
# status as 141 even though the match succeeded, so `&& echo true || echo false` takes the
# false branch. Measured over eight identical runs it alternates:
#
#     0 141 141 0 141 141 0 141
#
# **Whether the assertion passes depends on how much data the writer had buffered.** A gate
# built on that is not a flaky gate in the usual sense — it is a gate whose verdict is a
# function of the size of the file, and a gate that reports a defect the code does not have
# teaches the next writer to distrust the whole file. (The other half of the trap: the very
# same form is what a *negative* control is written with, and there "no match" is the pass, so
# SIGPIPE cannot reach it — which is why the failure looked selective and cost an hour.)
#
# Capturing the text first and then testing the variable has no writer to lose, so the
# result is deterministic. This is a gate measuring a `match` arm; the last thing it should
# be is a measurement of the shell.
has() { case "$2" in *"$1"*) echo true;; *) echo false;; esac; }
# `has_re PATTERN TEXT` — the regex form, via python rather than a pipeline.
has_re() { python3 -c '
import re,sys
try:
    print("true" if re.search(sys.argv[1], sys.argv[2], re.S) else "false")
except re.error:
    print("false")
' "$1" "$2"; }
# `grep_any PATTERN FILE...` — does any file contain a match, WITHOUT a pipeline. Returns
# true/false on stdout, never a status code a caller has to interpret.
grep_any() {
  local pattern="$1"; shift
  grep -lE "$pattern" "$@" 2>/dev/null | head -1 | grep -q . && echo true || echo false
}
# `grep_all PATTERN FILE...` — does EVERY file contain a match.
grep_all() {
  local pattern="$1"; shift
  local f
  for f in "$@"; do
    [ -f "$f" ] || { echo false; return; }
    grep -qE -- "$pattern" "$f" 2>/dev/null || { echo false; return; }
  done
  echo true
}

# `has_any NEEDLE... TEXT` — is any needle a literal substring of TEXT? A gate that counts
# arms by piping into `awk` puts a command substitution boundary in the middle of a pipeline,
# and a boundary there does not fail where you can see it.
has_any() {
  local text="${!#}" needle
  for needle in "${@:1:$#-1}"; do
    case "$text" in *"$needle"*) echo true; return;; esac
  done
  echo false
}

PASSED=0
FAILED=0
LEG=0
TOTAL=7

# The stripper refuses rather than degrading: this file's own docs QUOTE the arms they
# replaced, so the code under test contains the old spelling as a literal inside a comment,
# and a stripper that leaves comments in place would find the defect "fixed" the moment the
# comment quoting it is written. The balanced-comment requirement is the same as the sibling
# gate's: a stripper that eats its own subject produces a gate that measures nothing while
# looking like a gate, so the file is refused (exit 4) rather than measured.
strip_comments() {
  python3 - "$1" <<'PY'
import re, sys

path = sys.argv[1]
src = open(path, encoding="utf-8").read()

# Line comments first — a `//` comment hides a `/*` from the block scanner.
text = re.sub(r"//[^\n]*", "", src)

opens, closes = text.count("/*"), text.count("*/")
if opens != closes:
    sys.stderr.write(
        f"run-autoresponder-reason: {path} has {opens} '/*' and {closes} '*/' after line "
        f"comments are removed — a block comment is unbalanced, so every assertion below would "
        f"run against a truncated file.\n"
    )
    raise SystemExit(4)

out, i, depth = [], 0, 0
while i < len(text):
    if text.startswith("/*", i):
        depth += 1; i += 2; continue
    if text.startswith("*/", i) and depth:
        depth -= 1; i += 2; continue
    if depth == 0:
        out.append(text[i])
    i += 1

stripped = "".join(out)
if depth != 0 or not stripped.strip():
    sys.stderr.write(
        f"run-autoresponder-reason: stripping {path} left {len(stripped)} bytes — refusing to "
        f"measure a file the gate could not read.\n"
    )
    raise SystemExit(4)

# Drop the #[cfg(test)] module: an assertion's own argument must not satisfy its own gate.
m = re.search(r"#\[cfg\(test\)\]", stripped)
if m:
    stripped = stripped[: m.start()]
sys.stdout.write(stripped)
PY
}

for f in "$AUTORESPONDER" "$ROUTE" "$STORE" "$PANEL" "$LIB"; do
  if [ ! -f "$f" ]; then
    echo "run-autoresponder-reason: $f is missing — the gate is measuring nothing" >&2
    exit 4
  fi
done

A_CODE="$(strip_comments "$AUTORESPONDER")"
R_CODE="$(strip_comments "$ROUTE")"
S_CODE="$(strip_comments "$STORE")"
P_CODE="$(strip_comments "$PANEL")"

# The reason strings are the code under test, so the SQL/whitespace unfolding that the
# sibling gate needed is not needed here — but the arms are on separate lines with a
# pattern arm, so the two are read as a block rather than line by line.
# The reason arms are the code under test, so they are read as one collapsed line: the match
# is written across seven lines and a line-by-line grep would answer "this line has no arm"
# for six of them. Collapsing first is what makes the arms comparable to each other, which is
# the only way to ask "do two of them say the same word?".
reason_arm() {
  # The `tr` is load-bearing and must come FIRST. The arms are on seven lines with four
  # spaces of indentation each, and the helper below used to terminate the match on `\n    }`
  # — a newline that `tr` had already removed, so the regex never matched and the leg
  # reported `false` against a body that plainly separates the two words. Collapsing once,
  # here, is what lets the helper ask about ARMS rather than about whitespace.
  printf '%s' "$A_CODE" | tr '\n' ' ' | tr -s ' ' \
    | grep -oE 'pub fn reason\(&self\) -> &.static str \{.*?\}' \
    | head -1
}
REASON_BODY="$(reason_arm || true)"

# Do the arms say the sent word and the delayed word at DIFFERENT positions? This is the
# property, stated as a measurement rather than as a copy of the arms: a copy would be a
# second spelling of the rule, which is the thing that went wrong.
sent_and_delayed_differ() {
  python3 - "$1" <<'PYS'
import re, sys
body = sys.argv[1]
m = re.search(r"pub fn reason\(&self\) -> &.static str \{(.*?)\}", body, re.S)
if not m:
    print("false"); raise SystemExit
arms = re.findall(r'=>\s*"([a-z_]+)"', m.group(1))
sent = [i for i, a in enumerate(arms) if a == "sent"]
delay = [i for i, a in enumerate(arms) if a == "delayed"]
print("true" if sent and delay and sent != delay else "false")
PYS
}

# --------------------------------------------------------------------------------------------
# leg 1: the reason function exists and still has a caller outside its own test module
# --------------------------------------------------------------------------------------------
LEG=$((LEG + 1))
leg 1 "the reason function exists and has a production caller"
check "reason() is defined on Delivery" \
  "$(has_re 'pub fn reason\(&self\)' "$REASON_BODY")" \
  "no reason() body found in autoresponder.rs — the gate cannot read the rule it names"
check "it has a caller outside the test module" \
  "$(grep_any '\breason\b' "$ROUTE" "$STORE")" \
  "reason() has no caller in the route or the store — the function is documentation"

# --------------------------------------------------------------------------------------------
# leg 2: the delayed verdict is NOT the word "sent"
#
# This is the leg the whole gate exists for. The negative control on its own would also be
# satisfied by a function that had been deleted, so the POSITIVE control below asks for the
# word to still be reachable — from the arm that may have a mailer, and from no other.
# --------------------------------------------------------------------------------------------
LEG=$((LEG + 1))
leg 2 "a delayed verdict is not reported as sent"
no "the bare Ready arm does not say \"sent\"" \
  "$(has_re 'Self::Ready\(_\) => *"sent"' "$REASON_BODY")" \
  "Ready(_) => \"sent\" — a DELAYED Ready is a reservation, not a send"
check "the word \"sent\" is still produced, and only for an undelayed message" \
  "$(has_re 'Ready\(message\) if !message\.delayed => *"sent"' "$REASON_BODY")" \
  "no arm distinguishes an undelayed Ready from a delayed one — the negative control above "\
"would pass against a function that cannot say \"sent\" at all"
check "a delayed Ready answers \"delayed\"" \
  "$(has_re 'Ready\(_\) *\| *Self::NotYet\(_\) => *"delayed"' "$REASON_BODY")" \
  "the delayed case has no word of its own — it is answered by an arm that is unreachable"

# --------------------------------------------------------------------------------------------
# leg 3: the word "delayed" is REACHABLE, which is the half of the defect that was invisible
#
# `NotYet` is the variant whose name is the answer and which nothing constructs: `deliver`
# answers a delayed source with `Ready { delayed: true }` instead, because a `NotYet` carries
# no message for the worker to send. So before this slice the word "delayed" reached the
# trail only through an arm with no producer. The leg asks the question the enum actually
# poses — is the arm I can reach the arm I want? — and states it as a measurement, because
# "the delayed verdict has a word" is the property and "the variant was deleted" is not a
# way to obtain it.
# --------------------------------------------------------------------------------------------
LEG=$((LEG + 1))
leg 3 "the delayed verdict reaches a word of its own"
check "deliver() produces the delayed case as Ready { delayed: true }" \
  "$(has_re 'delayed: due_at\.is_some\(\)' "$A_CODE")" \
  "deliver() no longer sets delayed from due_at — the claim and the word disagree again"
check "the sent word and the delayed word are different arms" \
  "$(sent_and_delayed_differ "$REASON_BODY")" \
  "the reason arms do not separate \"sent\" from \"delayed\" — one word is naming two states"

# --------------------------------------------------------------------------------------------
# leg 4: the route's skip note carries the delay-aware word, not the unreachable one
# --------------------------------------------------------------------------------------------
LEG=$((LEG + 1))
leg 4 "the route does not write \"sent\" onto a claim it did not send"
check "the skip call passes the verdict's reason" \
  "$(has_re 'record_skip\([^)]*verdict\.reason\(\)' "$R_CODE")" \
  "record_skip no longer passes verdict.reason() — the gate cannot see what the trail stores"
# The route's own comment promises the two cases are told apart. The comment is the
# specification, so the gate reads it back: a branch that says it handles two cases and
# handles them with the same value is the defect, and the promise is in the file.
check "the skip arm's comment still promises the two cases are distinguished" \
  "$(has_re 'delayed message is the worker' "$(cat "$ROUTE")")" \
  "the route's comment about the delayed case is gone — if the guarantee moved, the gate "\
"must be told rather than left measuring a promise that no longer exists"

# --------------------------------------------------------------------------------------------
# leg 5: the panel does not render the raw reason beside the state chip
#
# `lead-detail.tsx` rendered `{event.detail.reason}` unconditionally, so a reserved claim read
# "sent" and then the state chip — the same fact twice, one of the two false. The chip is the
# authority because it is read from the stored row; `reason` is a word chosen at decision time
# and can disagree with what was recorded. The guard therefore belongs on the LINE (a claim
# line has a state, and its reason is redundant with it), not on the string.
# --------------------------------------------------------------------------------------------
LEG=$((LEG + 1))
leg 5 "the trail line does not print the raw reason next to the state"
check "the state chip exists" \
  "$(has 'data-autoresponder-state' "$P_CODE")" \
  "the panel no longer renders the autoresponder state — this gate's subject has moved"
check "the raw reason render is guarded by the claim check" \
  "$(has_re 'event\.detail\?\.reason === "string"\s*&&\s*!event\.autoresponder_state' "$P_CODE")" \
  "lead-detail.tsx still prints event.detail.reason with no guard — a reserved claim reads "\
"\"sent\" and \"Reserved\" on the same line"

# --------------------------------------------------------------------------------------------
# leg 6: the state vocabulary has a single owner, and the reason is not a second one
# --------------------------------------------------------------------------------------------
LEG=$((LEG + 1))
leg 6 "the state vocabulary has a single owner"
check "AUTORESPONDER_STATE_LABEL is a total map over the states" \
  "$(has_re 'AUTORESPONDER_STATE_LABEL[^;]*satisfies Record<AutoresponderState, string>' "$(cat "$LIB")")" \
  "the panel's state label map is not a total Record — a state it does not know renders blank"
check "the reason accessor still exists" \
  "$(has_re 'fn reason\(&self\)' "$A_CODE")" \
  "no reason accessor on Delivery at all — either this gate's subject is gone (say so) or "\
"the rule moved somewhere this gate cannot see"
check "verdict_name is still the editor's separate vocabulary" \
  "$(has_re 'fn verdict_name\(&self\)' "$A_CODE")" \
  "verdict_name() is gone — slice 45's editor vocabulary regressed while this leg passed"

# --------------------------------------------------------------------------------------------
# --------------------------------------------------------------------------------------------
# leg 7: the tenancy-free neighbours are untouched
#
# A gate whose every assertion is negative is satisfied by a file that says nothing. Each
# neighbour below is a claim the slice had no reason to change, and each is stated positively
# so that "the reason accessor was deleted" cannot turn the whole gate green.
# --------------------------------------------------------------------------------------------
LEG=$((LEG + 1))
leg 7 "tenancy-free neighbours are untouched"
check "the store still records the skip note with a reason" \
  "$(has 'insert("reason".to_string()' "$(cat "$STORE")")" \
  "record_skip no longer writes a reason into the note — the recovery trail lost its sentence"
check "ClaimState still has its five states" \
  "$(has_any 'Sent' 'Reserved' 'Claimed' 'Abandoned' 'Unknown' "$(cat "$STORE")")" \
  "the stored-claim state vocabulary lost a state — the slice that introduced it regressed"
check "the sendable rule is still the single owner of the send question" \
  "$(has 'pub fn sendable(' "$A_CODE")" \
  "sendable() is gone — slice 45's single owner regressed while this leg passed"

# --------------------------------------------------------------------------------------------
if [ "$FAILED" -ne 0 ]; then
  printf '\n\033[31mFAIL\033[0m %d/%d legs failed, %d/%d assertions passed\n' \
    "$FAILED" "$TOTAL" "$PASSED" "$((PASSED + FAILED))"
  exit 1
fi
if [ "$LEG" -ne "$TOTAL" ]; then
  # `set -e` never notices a conditional that was not taken, so a leg that silently never
  # ran would leave the summary reading PASS. The tick-68 shape, one layer out.
  printf '\n\033[31mFAIL\033[0m only %d of %d legs ran — a leg that never executed is not a pass\n' \
    "$LEG" "$TOTAL"
  exit 1
fi
printf '\n\033[32mPASS\033[0m %d legs, %d assertions\n' "$LEG" "$PASSED"
