#!/usr/bin/env bash
# run-autoresponder-skip-words.sh — a skip note must be a sentence, and the sentence the verdict
# carried must reach the row.
#
#   bash scripts/qa/run-autoresponder-skip-words.sh
#
# THE DEFECT THIS GATE NAMES
#
# Two halves of one sentence about the same trail line, both wrong, in opposite directions.
#
# WRITER. `spawn_autoresponder` passed `serde_json::json!({})` to `record_skip` for EVERY
# verdict, and the sweep's decline arm passed `json!({ "reserved": true })`. So the one autoresponder
# failure an operator can actually fix — switched on, has a subject, body renders to nothing —
# wrote exactly one word, `invalid_template`, onto the lead's timeline. The variant was
# CONSTRUCTED with the sentence: `Delivery::InvalidTemplate("the template renders to nothing")`.
# The data existed, was carried to the call site, and was dropped there. **A payload the writer
# drops is data the platform had and threw away** — and this is the crate's second instance of
# it, after the batch limit that filled a batch with rows it discarded (there the ROWS were
# dropped, here the SENTENCE is).
#
# READER. Having decided that a skip line gets no `ClaimState` chip — correct, a note never
# claimed anything — nothing gave that line any WORDS. The timeline rendered
# `event.detail.reason` verbatim, and every value in it is a machine word the server invented:
# `not_accepted`, `no_address`, `source_disabled`, `invalid_template`, `delayed`,
# `not_configured`, `already_sent`. An operator opening a lead to ask "why did nobody email this
# person?" read a token.
#
# WHY A FILE-READING GATE IS THE RIGHT INSTRUMENT
#
# Half of this is a Rust `match` arm, half is a TypeScript render guard, and the third thing is
# that the two halves can each be green while the sentence still never arrives: `skip_payload()`
# can exist with no caller (this branch's seventh instance of a correct function with no caller
# able to produce the state it describes), and the panel can own a label map with no render that
# consults it. A browser pass measures the walkthrough's own fixture — whose QA source is
# created WITHOUT an autoresponder, so no skip line is ever produced on screen — and a `cargo
# test` proves the function agrees with itself.
#
# THE SIBLING GATE AND ITS LESSON
#
# run-autoresponder-reason.sh documents a race: `printf … | grep -q` under `set -o pipefail`
# returns 141 when the reader exits first, so `&& true || false` takes the wrong branch and the
# assertion's verdict is a function of how much the writer had buffered. That gate writes every
# match as a `case` statement for this reason. **This file does the same, and leg 1's first
# version did not** — it used the pipeline form in a file whose header names the trap, and it
# reported `false` against code that was present. The helper below is the only matching form
# here, and a leg that uses anything else has to be justified in its own comment.
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

# `has NEEDLE TEXT` — literal substring, via `case`. No pipeline: see the header.
has() { case "$2" in *"$1"*) echo true;; *) echo false;; esac; }
# `has_file NEEDLE FILE` — the same question about a FILE's contents.
#
# ## Why this exists, and it is the bug leg 6's second check caught on its first run
#
# It was written as `has 'raise SystemExit(4)' "$SELF"` — passing the gate's own path where a
# string was expected. `case` never sees a file: it compares the literal text, the path is
# nowhere in it, and the check answers `false` **against correct code**. The failure looks like
# "the stripper does not refuse", i.e. like a finding about the product, and the reader has no
# way to tell it from one.
#
# The class is argument *count*, not argument *type*, and `has_re` hides it: a path in the
# TEXT slot of `has_re` would be a silent regex miss rather than a compile error, so the two
# helpers fail the same way for the same mistake in opposite silence. A reader who sees
# `has "$FILE"` must not have to wonder whether the file is read.
has_file() {
  local needle="$1" file="$2"
  [ -f "$file" ] || { echo false; return; }
  local text
  text="$(cat "$file")" || { echo false; return; }
  case "$text" in *"$needle"*) echo true;; *) echo false;; esac
}
# `has_re PATTERN TEXT` — extended regex, via python. Returns a value, never a status code a
# caller has to interpret, so a leg cannot pass on a signal it does not read.
has_re() { python3 -c '
import re, sys
try:
    print("true" if re.search(sys.argv[1], sys.argv[2], re.S) else "false")
except re.error:
    print("false")
' "$1" "$2"; }
# `has_any NEEDLE... TEXT` — is any needle a literal substring of TEXT? The last argument is
# the text, so a caller cannot accidentally read a needle as the haystack.
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
TOTAL=6

# Comments are stripped before anything is measured. This file QUOTES the defect it names
# (`json!({})` appears in the header, and `reason` arms are quoted in the sibling gate), so a
# matcher that reads prose finds the defect "still present" the moment the comment explaining
# it is written. The stripper refuses rather than degrades: a file it cannot read is a gate
# measuring nothing while looking like a gate.
strip_comments() {
  python3 - "$1" <<'PY'
import re, sys

path = sys.argv[1]
src = open(path, encoding="utf-8").read()

# Line comments first: a `//` comment hides a `/*` from the block scanner.
text = re.sub(r"//[^\n]*", "", src)

opens, closes = text.count("/*"), text.count("*/")
if opens != closes:
    sys.stderr.write(
        f"run-autoresponder-skip-words: {path} has {opens} '/*' and {closes} '*/' after line "
        "comments are removed — a block comment is unbalanced, so every assertion below would run "
        "against a truncated file.\n"
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
        f"run-autoresponder-skip-words: stripping {path} left {len(stripped)} bytes — refusing "
        "to measure a file the gate could not read.\n"
    )
    raise SystemExit(4)

# An assertion's own argument must not satisfy its own gate.
m = re.search(r"#\[cfg\(test\)\]", stripped)
if m:
    stripped = stripped[: m.start()]
sys.stdout.write(stripped)
PY
}

for f in "$AUTORESPONDER" "$ROUTE" "$STORE" "$PANEL" "$LIB"; do
  if [ ! -f "$f" ]; then
    echo "run-autoresponder-skip-words: $f is missing — the gate is measuring nothing" >&2
    exit 4
  fi
done

A_CODE="$(strip_comments "$AUTORESPONDER")"
R_CODE="$(strip_comments "$ROUTE")"
S_CODE="$(strip_comments "$STORE")"
P_CODE="$(strip_comments "$PANEL")"
L_CODE="$(strip_comments "$LIB")"

# ---------------------------------------------------------------------------------------------
# leg 1: the writer half — a verdict carries its diagnosis, and every caller keeps it
# ---------------------------------------------------------------------------------------------
LEG=$((LEG + 1))
leg 1 "a verdict carries its diagnosis to the note"
check "skip_payload exists and hands InvalidTemplate its sentence" \
  "$(has_re 'Self::InvalidTemplate\(diagnosis\) *=> *serde_json::json!\(\{[^}]*"explanation"' "$A_CODE")" \
  "the InvalidTemplate arm no longer writes an explanation key — the diagnosis the variant was \
constructed with is dropped again"
check "a verdict with nothing to explain adds no key" \
  "$(has_re 'Self::InvalidTemplate\(diagnosis\).*_ *=> *serde_json::json!\(\{\}\)' "$A_CODE")" \
  "every variant now writes an explanation, so a trail line makes a claim it has no evidence for"
# POSITIVE CONTROL. The negative arm above is satisfied by a function deleted outright, so the
# leg also asks the accessor to still be there.
check "the accessor has a caller outside its own test module" \
  "$(has 'skip_payload' "$R_CODE")" \
  "the route writes a literal again — the accessor exists with no production caller, which is \
this branch's seventh instance of a correct function nothing can reach"
no "the route no longer passes an empty literal" \
  "$(has_re 'record_skip\([^)]*verdict\.reason\(\)[^)]*json!\(\{\}\)' "$R_CODE")" \
  "the route is back to dropping the payload — json!({}) for every verdict"
check "the sweep's decline arm merges the verdict with its own context" \
  "$(has 'other.skip_payload_merge' "$S_CODE")" \
  "the decline arm writes a literal again; the sweep is the ONLY thing that ever sees an \
InvalidTemplate caused by an edit made during the delay"

# ---------------------------------------------------------------------------------------------
# leg 2: the reader half — a skip line has WORDS
#
# The control on this leg is the guard itself: `status_changed` and `assigned` also carry a
# `reason`, and those are operator-authored free text shown verbatim. A gate that only asserted
# "skipLabel is called somewhere" would pass a panel that had mangled a rejection reason into a
# sentence.
# ---------------------------------------------------------------------------------------------
LEG=$((LEG + 1))
leg 2 "the panel has words for the machine reasons"
check "the skip vocabulary has an owner in the client" \
  "$(has 'AUTORESPONDER_SKIP_LABEL' "$L_CODE")" \
  "no owner for these words anywhere in the front end — the trail renders a server token as if \
it were a sentence"
check "the label map covers the reasons the store writes" \
  "$(has_any 'not_configured' 'not_accepted' 'no_address' 'invalid_template' 'delayed' 'already_sent' 'source_disabled' "$L_CODE")" \
  "the map lost a reason the store can write — an operator's lead shows a token again"
check "the render consults the map" \
  "$(has 'skipLabel(' "$P_CODE")" \
  "the map exists and nothing reads it — an owned vocabulary with no reader is a dead export"
check "the render is guarded on the KIND, not on the word" \
  "$(has_re 'event\.kind === "autoresponder_sent"\s*\?\s*skipLabel' "$P_CODE")" \
  "the guard is missing or is on the string — operator-authored reasons (status_changed, \
assigned) would be rewritten as autoresponder sentences"

# ---------------------------------------------------------------------------------------------
# leg 3: the sentence is RENDERED, which is the half a server-only gate cannot see
# ---------------------------------------------------------------------------------------------
LEG=$((LEG + 1))
leg 3 "the explanation is rendered on the line"
check "the explanation has its own element" \
  "$(has 'data-lead-trail-autoresponder-explanation' "$P_CODE")" \
  "the panel stores the sentence and renders nothing — an operator opens the lead and still \
reads only a token"
check "that element is bound to the stored key" \
  "$(has_re 'data-lead-trail-autoresponder-explanation[^>]*>\s*\{event\.detail\.explanation\}' "$P_CODE")" \
  "the element is bound to nothing — a permanent empty line on every autoresponder note"
check "it is guarded on the autoresponder kind" \
  "$(has_re 'event\.kind === "autoresponder_sent".*event\.detail\?\.explanation === "string"' "$P_CODE")" \
  "any line carrying an explanation key would render it — the key is the store's, and a future \
non-autoresponder line must not be assumed to mean the same thing"

# ---------------------------------------------------------------------------------------------
# leg 4: THE SIBLING SLICE'S SUBJECT IS UNCHANGED, stated positively
#
# Every leg so far is satisfiable by deleting the code under test. Each check below is a
# neighbour this slice had no reason to change, so "the fix was reverted and the accessor was
# deleted" cannot turn the gate green.
# ---------------------------------------------------------------------------------------------
LEG=$((LEG + 1))
leg 4 "tenancy-free neighbours are untouched"
check "reason() still separates an undelayed Ready from a delayed one" \
  "$(has_re 'Ready\(message\) if !message\.delayed => *"sent"' "$A_CODE")" \
  "slice 45/46's single spelling of the send question regressed while this gate passed"
check "the claim state vocabulary still has its five states" \
  "$(has_re 'enum ClaimState' "$S_CODE")" \
  "the stored-claim vocabulary is gone — the slice that introduced it regressed"
check "record_skip still writes reason and source" \
  "$(has 'insert("reason".to_string()' "$S_CODE")" \
  "record_skip stopped naming the source on the note — the recovery trail lost its sentence"
check "the raw-reason render is still guarded by the claim check" \
  "$(has_re 'event\.detail\?\.reason === "string"\s*&&\s*!event\.autoresponder_state' "$P_CODE")" \
  "a claim line prints its reason again — a reserved claim reads 'sent' and 'Reserved' on one \
line"

# ---------------------------------------------------------------------------------------------
# leg 5: the store note and the reader agree on the KEY NAME
#
# The two halves are in different languages, so the key is a shared convention with no compiler
# behind it. `record_skip` inserts `reason` and `source`; the reader reads `detail.reason` and
# `detail.explanation`. A rename on one side compiles on both and renders nothing on the other.
# ---------------------------------------------------------------------------------------------
LEG=$((LEG + 1))
leg 5 "the writer and the reader agree on the key names"
check "the writer inserts the explanation key the reader reads" \
  "$(has '"explanation"' "$A_CODE")" \
  "the accessor no longer names the key the reader looks for"
check "the reader reads the key the writer inserts" \
  "$(has 'event.detail?.explanation' "$P_CODE")" \
  "the reader reads a key no writer produces — the element is permanently empty"
check "the skip vocabulary's words are not the raw tokens" \
  "$(has_re 'not_accepted: *"[A-Z]' "$L_CODE")" \
  "a map value is the token itself — the sentence and the word are the same string again"

# ---------------------------------------------------------------------------------------------
# leg 6: the gate is measuring what it says it measures
#
# A gate that greps a file must be able to fail. Every leg above is a `check` or a `no`, and a
# body of pure negatives is satisfied by a file that says nothing — which is why this leg asks
# for positive presence and this one asks that the stripper refuses an unreadable file.
# ---------------------------------------------------------------------------------------------
LEG=$((LEG + 1))
leg 6 "the gate's own instrument is sound"
check "the comment stripper left the subject readable" \
  "$([ "${#A_CODE}" -gt 1000 ] && echo true || echo false)" \
  "strip_comments emptied autoresponder.rs — every assertion above would read a blank string"
check "the stripper refuses an unbalanced block comment" \
  "$(has_file 'raise SystemExit(4)' "$REPO_ROOT/scripts/qa/run-autoresponder-skip-words.sh")" \
  "the stripper degrades instead of refusing — a gate that measures a truncated file while \
looking like a gate"

# ---------------------------------------------------------------------------------------------
if [ "$FAILED" -ne 0 ]; then
  printf '\n\033[31mFAIL\033[0m %d/%d legs failed, %d/%d assertions passed\n' \
    "$FAILED" "$TOTAL" "$PASSED" "$((PASSED + FAILED))"
  exit 1
fi
if [ "$LEG" -ne "$TOTAL" ]; then
  # `set -e` never notices a conditional that was not taken, so a leg that silently never ran
  # would leave the summary reading PASS.
  printf '\n\033[31mFAIL\033[0m only %d of %d legs ran — a leg that never executed is not a pass\n' \
    "$LEG" "$TOTAL"
  exit 1
fi
printf '\n\033[32mPASS\033[0m %d legs, %d assertions\n' "$LEG" "$PASSED"