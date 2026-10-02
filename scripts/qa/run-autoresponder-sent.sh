#!/usr/bin/env bash
# run-autoresponder-sent.sh — "the autoresponder was sent" must mean the same thing in every
# caller, and a *delayed* message is reserved, not sent.
#
# THE DEFECT THIS GATE NAMES
#
# `Outcome::sent()` (autoresponder_store.rs) answers "was this handed to the mailer?" with
#
#     matches!(self.verdict, Delivery::Ready(_))
#
# and its doc comment says exactly that: "`true` when a message actually went to the mailer."
# It is wrong for the delayed case, and wrong in the direction that hides work.
#
# `prepare()` takes the claim for EVERY ready message, delayed or not — that is deliberate and
# documented, because the claim is what makes the delay happen later instead of never. So a
# source with a 30-minute send delay returns `Outcome { verdict: Ready { delayed: true } }`
# and `sent()` answers TRUE, while the thing that actually mails the lead is the due-worker
# (`due_reservations`) and no mailer has been touched. The route's own send path gets this
# right — `crm_intake.rs` matches `Delivery::Ready(message) if !message.delayed` — which is
# what makes the copy dangerous: a reader who checks the only *working* call site concludes the
# simple `matches!` is the same rule, and it is not.
#
# There is a THIRD spelling of the same question. `crm_intake.rs` has a hand-written
#
#     fn verdict_name(delivery: &Delivery) -> &'static str { match delivery { Ready(_) => "ready", ... } }
#
# that re-lists every variant in order, and `Delivery::is_sendable()` — the method whose own doc
# says "the caller treats every other variant as 'nothing went out'" — has ZERO callers in the
# repository. A method whose documentation describes a caller that does not exist is the
# "capability with no caller" shape this branch has met seven times, and here it was pointing
# at a real disagreement rather than at nothing.
#
# WHY A PURE-FUNCTION GATE IS THE RIGHT INSTRUMENT
#
# The question is a function of a `Delivery` and a delay flag — no database needed, and a gate
# that opened a socket here would be measuring the mailer, not the rule. The rule is what three
# call sites have to agree on, and three call sites can be enumerated by hand in a way three
# HTTP requests cannot.
#
# The assertions are ABOUT THE PRODUCT SOURCE, not about a Rust function called from this
# script: a `cargo test` that calls the function proves the function agrees with itself, which is
# the tick-68 shape (a fixture measuring its own author). These read the shipped files, so a
# later writer who re-spells the rule is caught here rather than in production.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

AUTORESPONDER="modules/crm-intake/src/autoresponder.rs"
STORE="modules/crm-intake/src/autoresponder_store.rs"
ROUTE="apps/api/src/routes/crm_intake.rs"

leg()  { printf 'leg %-46s %s\n' "$1" "$2"; }
pass() { printf '  \033[32mPASS\033[0m %s\n' "$1"; PASSED=$((PASSED + 1)); }
fail() { printf '  \033[31mFAIL\033[0m %s — %s\n' "$1" "$2"; FAILED=$((FAILED + 1)); }
check() { if [ "$2" = "true" ]; then pass "$1"; else fail "$1" "$3"; fi; }
notes() { printf '      note: %s\n' "$1"; }

PASSED=0
FAILED=0
LEG=0
TOTAL=8

strip_comments() {
  # Comments are the half that must go: this crate's own docs QUOTE the statements they
  # replaced, on purpose. Stripping string literals as well would make the assertions
  # unsatisfiable, because the code under test *is* the literal.
  #
  # **Line comments are stripped FIRST, and the order is load-bearing, not taste.** The first
  # version scanned for `/*` first and got a count of 2 opens and 1 close in `crm_intake.rs` —
  # a file with no unbalanced comment at all. Line 1 is `//! ... /api/v1/crm/intake/*`, so the
  # trailing `/*` of a URL path inside a `//!` doc comment opened a block comment that ran to
  # the end of the file. The guard fired and said "unbalanced", which was true of the scan and
  # false of the file, and the fix is the lexer's order: a `//` comment hides a `/*` from the
  # block scanner, exactly as a compiler's lexer would.
  #
  # **The balanced-comment requirement is the gate's own correctness, not a nicety.** With the
  # counter never returning to zero, every character after the phantom `/*` was discarded, the
  # three "read the file" variables came back EMPTY, and every assertion ran against "" — which
  # is why four legs printed verdicts and two of them printed PASS. A stripper that eats its
  # own subject produces a gate that measures nothing while looking like a gate. So the file is
  # REFUSED (exit 4) rather than measured, and the refusal is a non-zero exit rather than a
  # warning: **a gate that cannot read its own subject must stop, not continue with an empty
  # string**, because `grep` on an empty file answers "no match" for every assertion and half of
  # them are written so that "no match" reads green.
  python3 - "$1" <<'PY'
import re, sys

path = sys.argv[1]
src = open(path, encoding="utf-8").read()

# 1. Line comments first — they are what hides a `/*` from step 2.
text = re.sub(r"//[^\n]*", "", src)

# 2. Then block comments, which must balance or the scan is measuring nothing.
opens, closes = text.count("/*"), text.count("*/")
if opens != closes:
    sys.stderr.write(
        f"run-autoresponder-sent: {path} has {opens} '/*' and {closes} '*/' after line "
        f"comments are removed — a block comment is unbalanced, so every assertion below "
        f"would run against a truncated file.\n"
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
        f"run-autoresponder-sent: stripping {path} left {len(stripped)} bytes — refusing to "
        f"measure a file the gate could not read.\n"
    )
    raise SystemExit(4)

# 3. Drop the #[cfg(test)] module so an assertion's own argument cannot satisfy its own gate.
m = re.search(r"#\[cfg\(test\)\]", stripped)
if m:
    stripped = stripped[: m.start()]
sys.stdout.write(stripped)
PY
}

for f in "$AUTORESPONDER" "$STORE" "$ROUTE"; do
  if [ ! -f "$f" ]; then
    echo "run-autoresponder-sent: $f is missing — the gate is measuring nothing" >&2
    exit 4
  fi
done

A_CODE="$(strip_comments "$AUTORESPONDER")"
S_CODE="$(strip_comments "$STORE")"
R_CODE="$(strip_comments "$ROUTE")"

# --------------------------------------------------------------------------------------------
# leg 1: the helper exists and is reachable from outside its own module
# --------------------------------------------------------------------------------------------
LEG=$((LEG + 1))
check "the sendable helper is public" \
  "$(printf '%s' "$A_CODE" | grep -qE 'pub fn (is_sendable|sendable)\(' && echo true || echo false)" \
  "no public is_sendable/sendable on Delivery — the question has no single name"
check "it has a caller outside the test module" \
  "$(grep -rnw --include=*.rs --include=*.tsx -e is_sendable -e sendable "$STORE" "$ROUTE" apps/admin 2>/dev/null | grep -q . && echo true || echo false)" \
  "Delivery's own doc describes 'the caller' that treats every other variant as nothing went out, and there is none"
notes "a doc comment naming a caller that does not exist is a promise to a future reader"

# --------------------------------------------------------------------------------------------
# leg 2: 'sent' is not the same question as 'ready' — the delay is the whole defect
# --------------------------------------------------------------------------------------------
LEG=$((LEG + 1))
# The store's answer must consult the delay, not just the discriminant.
SENT_BODY="$(printf '%s' "$S_CODE" | sed -n '/pub fn sent(&self) -> bool/,/^    }/p')"
# **The assertion asks whether the answer is DELEGATED, not whether a literal survived.**
# The first version of this leg required the text `delayed` to appear inside `sent()`'s body,
# which is true of a fix that inlines the rule and false of a fix that routes it through
# `Delivery::is_sendable` — the shape this gate exists to produce. A gate that demands the
# previous fix's spelling refuses the better one, and the pressure it creates is to inline it
# again. **Assert the property (one owner of the rule), never the expression that happened to
# carry it.**
check "Outcome::sent is answered by the one rule, not its own" \
  "$(printf '%s' "$SENT_BODY" | grep -qE '(is_sendable|sendable)\(' && echo true || echo false)" \
  "sent() still matches Delivery::Ready itself, so it answers true for a DELAYED message - a reservation is not a send, and the name says it went to the mailer"
notes "prepare() claims a delayed message on purpose; the claim is what makes the delay happen later"

# **The product's own send path must go through the accessor too, and the accessor must
# actually exclude a delay.** Two assertions rather than one, because either can rot alone: a
# route that re-inlines `if !message.delayed` passes the first and fails the second, and an
# accessor that lost the delay passes both and reintroduces the original defect.
# **The raw file, not the stripped one — and this is the third time on this gate.** The `//`
# regex runs before the block scan (it must, or a `/*` inside a `//!` doc comment opens a
# comment that never closes), and the two together happen to cut the *span* between the first
# `//` and the next newline off every line. That is correct for a line comment and destructive
# for anything this assertion needs, because the three call sites are formatted across lines
# and the one that survives stripping is not the one the grep looks for. Assertions about
# CALLING CONVENTION read the raw file; assertions about a rule's BODY read the stripped one,
# because only the body can be satisfied by a comment.
check "the route's send path asks the accessor" \
  "$(grep -qE '(outcome|delivery)\.sendable\(\)' "$ROUTE" && echo true || echo false)" \
  "the route re-spells 'may this go to the mailer' with a local match - a copy is what drifts"
check "the one rule still excludes a delayed message" \
  "$(printf '%s' "$A_CODE" | sed -n '/pub fn sendable(&self)/,/^    }/p' | grep -q 'delayed' && echo true || echo false)" \
  "Delivery::sendable hands a DELAYED message back, so every caller of the one rule is wrong at once"

# --------------------------------------------------------------------------------------------
# leg 3: one spelling of the variant, not three
# --------------------------------------------------------------------------------------------
LEG=$((LEG + 1))
READY_ARMS="$(printf '%s\n%s' "$S_CODE" "$R_CODE" | grep -c 'Delivery::Ready' || true)"
NAMES_BODY="$(printf '%s' "$R_CODE" | sed -n '/fn verdict_name(/,/^}/p')"
check "verdict_name is not a hand-written discriminant copy" \
  "$(if [ -n "$NAMES_BODY" ]; then echo false; else echo true; fi)" \
  "crm_intake.rs re-lists every Delivery variant in a local match — the third spelling of one question"
notes "a hand-written copy of an enum's arms does not fail to compile when a variant is added"

# The store must not be a fourth copy.
check "the store reads the helper rather than re-matching" \
  "$(printf '%s' "$S_CODE" | grep -qE 'verdict\.(is_sendable|sendable)\(' && echo true || echo false)" \
  "autoresponder_store.rs matches Delivery::Ready itself instead of asking the one helper"
notes "the discriminating direction: the arms of a match are the part that goes stale; the variant test is stable"

# --------------------------------------------------------------------------------------------
# leg 4: the delay is representable at all — the negative control of leg 2
# --------------------------------------------------------------------------------------------
LEG=$((LEG + 1))
check "the delayed arm still exists on Ready" \
  "$(printf '%s' "$A_CODE" | grep -qE 'delayed: bool' && echo true || echo false)" \
  "Message has no delayed field, so leg 2's rule has nothing to consult and the defect cannot exist"
check "a delayed message carries a due instant" \
  "$(printf '%s' "$A_CODE" | grep -qE 'due_at: Option<OffsetDateTime>' && echo true || echo false)" \
  "without due_at there is no reservation to complete later, and the delay is dead rather than wrong"

# --------------------------------------------------------------------------------------------
# leg 5: the 'already sent' rule is the same rule in one place
# --------------------------------------------------------------------------------------------
LEG=$((LEG + 1))
check "was_sent asks about the sent key, not about a claim" \
  "$(printf '%s' "$S_CODE" | grep -qE 'detail\.get\("sent"\)' && echo true || echo false)" \
  "reading 'is there a claim' instead of 'was it sent' records a failed send as an answered lead forever"
# **This assertion reads the RAW file, not the stripped one, and the reason is the stripper.**
# The SQL is a Rust *string literal*, and the line-comment regex (`//[^\n]*`) is applied before
# the block-comment scan -- so it cannot eat it, but it also cannot help here: the check needs a
# `?` and a quoted key, and the earlier draft spelled the pattern with a shell double-quote
# around `detail ? 'sent'`, which the shell happily mangled into an empty alternation. The
# assertion then reported FAIL against code that is correct and unchanged since slice 1.
# **A gate whose pattern went through another quoting layer is testing the quoting.** The raw
# file is the subject here: the query is data, and no comment-stripping rule should be able to
# change a data assertion.
check "the claim read asks for the sent key" \
  "$(grep -qF "detail ? 'sent'" "$STORE" && echo true || echo false)" \
  "existing_claim returns the newest line of this kind, and record_skip writes the same kind with no sent key"

# --------------------------------------------------------------------------------------------
# leg 6: the rule survives a rename of the method
# --------------------------------------------------------------------------------------------
LEG=$((LEG + 1))
# If the helper were renamed, does anything still ask it?  This is the "the export is the
# evidence" leg: it is what stops a future tick from deleting the helper as dead code, which is
# precisely what nearly happened here.
HELPER_USERS="$(grep -rn --include=*.rs -e '\.is_sendable()' -e '\.sendable()' \
  "$STORE" "$ROUTE" modules/crm-intake/tests 2>/dev/null | grep -vc 'fn is_sendable\|fn sendable' || true)"
check "the helper is asked from at least one place" \
  "$(if [ "$HELPER_USERS" -ge 1 ]; then echo true; else echo false; fi)" \
  "the only evidence that 'sent' and 'ready' are different questions is a comment; delete the helper and nothing goes red"

# --------------------------------------------------------------------------------------------
# leg 7: the claim is taken for a delayed message, or the delay is a dead control
# --------------------------------------------------------------------------------------------
LEG=$((LEG + 1))
PREP_BODY="$(printf '%s' "$S_CODE" | sed -n '/pub async fn prepare(/,/^}/p')"
check "prepare claims a delayed message" \
  "$(printf '%s' "$PREP_BODY" | grep -qE 'if let Delivery::Ready' && echo true || echo false)" \
  "prepare gates its claim on an undelayed message — a source with any send delay then reserves nothing and the visitor is never answered"
notes "the earlier version of this code did exactly that, and every source left at zero delay kept working"

# --------------------------------------------------------------------------------------------
# leg 8: the worker that finishes a delayed send is actually reachable
# --------------------------------------------------------------------------------------------
LEG=$((LEG + 1))
DUE_BODY="$(printf '%s' "$S_CODE" | sed -n '/pub async fn due_reservations(/,/pub /p')"
check "due_reservations is a real sweep" \
  "$(if [ -n "$DUE_BODY" ] && printf '%s' "$DUE_BODY" | grep -qE 'select|fetch_all'; then echo true; else echo false; fi)" \
  "the function that makes a delay mean something reads nothing"
check "the worker that calls it exists" \
  "$(grep -rn --include=*.rs 'due_reservations' apps/api/src | grep -q . && echo true || echo false)" \
  "nothing drives the reservation sweep, so a delayed autoresponder is a control that never fires"
notes "a delay nobody completes is worse than no delay: the operator watches it work on every other source"

echo
echo "run-autoresponder-sent: $LEG legs run, $PASSED passed, $FAILED failed"
# The summary is the one line a reader trusts, so it says FAILED when a leg failed. An earlier
# version printed the count of legs that RAN under the word "failed" — two of them here, on a
# green file — which is the tick-76/79 shape (a verdict that contradicts the lines above it).
if [ "$LEG" -ne "$TOTAL" ]; then
  echo "run-autoresponder-sent: only $LEG of $TOTAL legs ran — a conditional that was not taken is not a pass" >&2
  exit 2
fi
[ "$FAILED" -eq 0 ] || exit 1
echo "PASS"
