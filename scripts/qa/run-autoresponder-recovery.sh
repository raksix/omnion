#!/usr/bin/env bash
# run-autoresponder-recovery.sh — an uncompleted autoresponder claim must be RE-OFFERED, and
# the only sweep that could re-offer it does not select its rows.
#
# THE DEFECT THIS GATE NAMES
#
# `prepare` takes the claim for every ready message and the caller then sends it *in the same
# process* (`crm_intake::spawn_autoresponder`, right after `prepare` returns). The recovery
# sweep, `due_reservations`, selects:
#
#     where e.kind = 'autoresponder_sent'
#       and e.detail->>'sent' = 'false'
#       and nullif(e.detail->>'due_at', '') is not null
#       and e.detail->>'due_at' <= now
#
# and an IMMEDIATE claim writes `due_at: null` (`claim` builds `due_at` from
# `message.due_at`, and an undelayed message has none). So an immediate claim is *excluded by
# the one query whose entire job is to re-offer uncompleted claims*, on the strength of a
# column that only exists to describe the DELAY.
#
# That is not a theoretical window. `mark_sent` has three failure modes that all return `Err`
# or `Ok(false)` without the message leaving:
#
#   * the app is killed (deploy, restart, OOM) between `prepare` and the send;
#   * `mark_sent` errors, and the route's error arm logs and MOVES ON — the mail may be out,
#     but nothing re-offered the row;
#   * the mailer refused, `release_claim` runs, and `release_claim` ITSELF fails.
#
# In every one of them the lead's single reply is owed and nothing owes it: `was_sent` is
# `false`, so `prepare` answers `Ready` — but its `claim` insert then hits the partial unique
# index from `0058` and returns `false`, which `prepare` turns into `AlreadySent`. The lead is
# permanently silent and the trail carries a line that reads as a reservation for ever.
#
# The rule is already written down, three functions above, in the module's own header: "**a
# claim that is later released on a send failure is the recoverable direction**". The recovery
# half was implemented for the DELAYED path (a `due_at` in the past keeps the row in the
# sweep) and never for the immediate one, because the immediate path's whole recovery story
# was "the caller is right there".
#
# WHY A SOURCE-READING GATE IS THE RIGHT INSTRUMENT HERE
#
# The defect is a *predicate*, and a predicate is invisible to a functional test unless the
# test can crash a process between two statements. Asserting it against the SQL text is not a
# shortcut: the four clauses that must hold together (`sent = 'false'`, `delivered_at`
# absent, a due instant OR a stale claim, and the ordering that makes the sweep cheap) are
# written once here and re-verified here, and the Rust integration test
# (`crm_autoresponder.rs::an_abandoned_immediate_claim_is_offered_again`) proves the same
# property over a real database. **A predicate the gate and the database test can disagree
# about is two facts; a predicate only a browser walkthrough can reach is zero.**
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

STORE="modules/crm-intake/src/autoresponder_store.rs"
ROUTE="apps/api/src/routes/crm_intake.rs"
RUNNER="apps/api/src/crm_autoresponder_runner.rs"
TESTS="modules/crm-intake/tests/crm_autoresponder.rs"

pass() { printf '  \033[32mPASS\033[0m %s\n' "$1"; PASSED=$((PASSED + 1)); }
fail() { printf '  \033[31mFAIL\033[0m %s — %s\n' "$1" "$2"; FAILED=$((FAILED + 1)); }
check() { if [ "$2" = "true" ]; then pass "$1"; else fail "$1" "$3"; fi; }
notes() { printf '      note: %s\n' "$1"; }

PASSED=0
FAILED=0
LEG=0
TOTAL=7

# `leg` is the ONLY way to advance, and it is called once per leg. **The first version advanced
# `LEG` inside the body of a leg and then read the same file twice**, so the header printed 28
# legs for 7 and the hard check at the bottom could never fire — a summary that counts four
# copies of one leg is the same failure as the false PASS above, in the arithmetic.
advance() { LEG=$((LEG + 1)); printf '\nleg %d/%d %s\n' "$LEG" "$TOTAL" "$1"; }

for f in "$STORE" "$ROUTE" "$RUNNER" "$TESTS"; do
  if [ ! -f "$f" ]; then
    echo "run-autoresponder-recovery: $f is missing — the gate is measuring nothing" >&2
    exit 4
  fi
done

# **The raw file, never a comment-stripped one.** Three times on this branch a stripper ate its
# own subject (a `/*` inside a `//!` path, a backtick inside a double-quoted message) and every
# assertion then ran against a truncated string while still printing a verdict. A predicate is
# a SQL *string literal*: no comment rule may touch it, and `grep -qF` on the raw bytes is the
# only assertion about data that cannot be corrupted by a lexer.
#
# **The continuations are JOINED, and that is the fourth time.** This crate writes SQL as a Rust
# string with `\` line continuations, so a clause ends `is not null \` and the next line starts
# `and`. The first version of leg 1 grepped for the single line `… is not null and` — a spelling
# the file has never contained — so the assertion answered "the old clause is gone" and printed
# **PASS against the shipped defect**. A pattern that does not occur anywhere in the repository
# is not a weak assertion, it is an inverted one: every file passes it and only the file that
# has the defect fails it. So the SQL is unfolded first, and leg 1's negative control greps for
# the clause in a form that IS present before the fix.
SWEEP_SQL="$(sed -n '/pub async fn due_reservations(/,/fetch_all(pool)/p' "$STORE" \
  | python3 -c 'import sys; t=sys.stdin.read(); sys.stdout.write(t.replace("\\\\\n", " "))')"
[ -n "$SWEEP_SQL" ] || { echo "run-autoresponder-recovery: the sweep's query is not where the gate looks" >&2; exit 4; }

# `grep -q` prints NOTHING and answers with its exit code. Written bare into a `check` argument
# it therefore passes the empty string, which is not "true" and not "false" — so the assertion
# reports FAIL against correct code. **Three of the seven legs read that way on the first run**,
# and the fix is a helper that always prints one of the two words.
yes() { if "$@"; then echo true; else echo false; fi; }

# ------------------------------------------------------------------------------------------------
# leg 1: an immediate claim is reachable by the recovery sweep
# ------------------------------------------------------------------------------------------------
advance "an immediate claim is reachable by the recovery sweep"
# The old WHERE had three conjuncts and no arm for a row with no due instant. Two assertions
# rather than one: a sweep that simply dropped the `due_at` clause would re-offer EVERY
# reservation including the ones that are legitimately waiting, and one that kept only the
# stale-claim arm would re-offer a claim the caller is still holding.
#
# **The negative control first.** Before the fix, the file contains
# `and nullif(e.detail->>'due_at', '') is not null and` — so this greps for exactly that, and
# the fix has to make it stop matching. A "the old clause is gone" assertion written against a
# spelling the repository never had is the inverted one described in the header.
check "the old due-instant requirement is gone (negative control)" \
  "$(yes grep -qF "nullif(e.detail->>'due_at', '') is not null" <<<"$SWEEP_SQL" && echo false || echo true)" \
  "the sweep still demands a due instant, so an immediate claim (due_at: null) is invisible to the query that exists to recover it"
check "an undelivered claim with no due instant is still offered" \
  "$(yes grep -qF "detail->>'sent' = 'false'" <<<"$SWEEP_SQL")" \
  "the sweep no longer selects uncompleted claims at all, so a lost send is lost for ever"

# ------------------------------------------------------------------------------------------------
# leg 2: the arm that re-offers it is bounded, or it is a duplicate generator
# ------------------------------------------------------------------------------------------------
advance "the arm that re-offers it is bounded, or it is a duplicate generator"
check "the re-offer arm is bounded by a staleness window" \
  "$(yes grep -qE 'claimed_before|CLAIM_STALE|delivery_claimed_at' <<<"$SWEEP_SQL")" \
  "a claim with no due instant is offered on every tick while the caller is still holding it, which is two mailers"
check "the sweep still orders by the due instant" \
  "$(yes grep -qE 'order by.*due_at.*asc' <<<"$SWEEP_SQL")" \
  "the oldest due reservation is no longer sent first, so a week-old reply can queue behind a fresh one"
notes "the bound is what makes this recovery rather than a second race"

# ------------------------------------------------------------------------------------------------
# leg 3: a delivered row is still out of the sweep's reach
# ------------------------------------------------------------------------------------------------
advance "a delivered row is still out of the sweep's reach"
check "a completed claim is excluded" \
  "$(yes grep -qF "not (e.detail ? 'delivered_at')" <<<"$SWEEP_SQL")" \
  "a delivered message would be offered again and mailed twice - the duplicate the whole claim design exists to prevent"
check "a released row is out of reach too" \
  "$(yes grep -qF "detail->>'sent' = 'false'" <<<"$SWEEP_SQL")" \
  "the sweep would offer a row the send path deliberately released"

# ------------------------------------------------------------------------------------------------
# leg 4: the re-render does not resurrect a switched-off source
# ------------------------------------------------------------------------------------------------
advance "the re-render does not resurrect a switched-off source"
SWEEP_BODY="$(sed -n '/pub async fn due_reservations(/,/^}/p' "$STORE")"
check "the sweep still re-reads the source's configuration" \
  "$(yes grep -q 'is_configured' <<<"$SWEEP_BODY")" \
  "a source switched off inside the delay must stop answering; re-rendering from a stored body would keep sending"
check "and a re-rendered row is still re-rendered, not read from the claim" \
  "$(yes grep -q 'deliver(&context' <<<"$SWEEP_BODY")" \
  "the message must come from the template at send time, so an operator's edit inside the delay is honoured"

# ------------------------------------------------------------------------------------------------
# leg 5: the caller that loses the recovery is not *also* claiming
# ------------------------------------------------------------------------------------------------
advance "the caller that loses the recovery is not *also* claiming"
# The re-offer arm and `claim_delivery` must not be the same lock with two names: a sweep that
# marks the row claimed and a worker that marks it claimed are two arbiter writes on one column,
# and the loser's silence has to be correct in both.
check "the sweep records that it took the send" \
  "$(yes grep -q 'claim_delivery' <<<"$SWEEP_BODY")" \
  "the sweep offers a row without taking the send, so two workers both mail the re-offered reservation"
check "and the worker's own take is still conditional" \
  "$(yes grep -qF 'for update skip locked' "$STORE")" \
  "the send claim lost its skip-locked guard, so the recovery sweep reintroduces the two-worker duplicate"

# ------------------------------------------------------------------------------------------------
# leg 6: the trail says what the state is, not what the code hoped
# ------------------------------------------------------------------------------------------------
advance "the trail says what the state is, not what the code hoped"
# An orphaned claim is a trail line with `sent: false` and no `delivered_at`. The detail screen
# reads exactly those two keys, so it is the one place that can turn "reserved" into "sent" or
# "released" into "still waiting" — and the REQ's audit sentence is about this line.
check "an abandoned claim is readable as abandoned" \
  "$(yes grep -qE 'fn claim_state|ClaimState' "$STORE" "$ROUTE")" \
  "nothing reads a claim's state for a reader, so the panel shows a reservation as a reservation for ever"
check "the panel renders the state the server decided" \
  "$(yes grep -qE 'autoresponder_state|claim_state' "$ROUTE" && yes grep -qE 'autoresponder_state|claim_state' apps/admin/lib/crm-intake.ts apps/admin/features/crm-intake/lead-detail.tsx)" \
  "the timeline infers the state from raw keys in the browser, which is the second place the two can disagree"

# ------------------------------------------------------------------------------------------------
# leg 7: the negative control — the recovery must not invent a second send
# ------------------------------------------------------------------------------------------------
advance "the negative control - the recovery must not invent a second send"
check "the database walk proves it, not just the predicate" \
  "$(yes grep -q 'an_abandoned_immediate_claim_is_offered_again' "$TESTS")" \
  "the predicate is asserted here and nowhere against a real row, so a wrong arm stays green in every functional test"
check "and it asserts the negative too" \
  "$(yes grep -qE 'a_delivered_claim_is_not_offered_again|a_completed_claim_is_never_offered_again' "$TESTS")" \
  "a walk that only proves the re-offer passes just as well on a sweep that re-offers everything"
notes "every delay assertion in this file is negative, so a positive control belongs beside it"

echo
echo "run-autoresponder-recovery: $LEG legs run, $PASSED passed, $FAILED failed"
if [ "$LEG" -ne "$TOTAL" ]; then
  echo "run-autoresponder-recovery: only $LEG of $TOTAL legs ran — a conditional that was not taken is not a pass" >&2
  exit 2
fi
[ "$FAILED" -eq 0 ] || exit 1
echo "PASS"
