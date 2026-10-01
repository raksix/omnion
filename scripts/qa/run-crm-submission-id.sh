#!/usr/bin/env bash
# CRM intake — ONE rule about a submission id, and who enforces it.
#
#   QA_CARGO_TARGET_DIR=/dev/shm/w8-target bash scripts/qa/run-crm-submission-id.sh
#
# ## Why this gate is its own file
#
# The claim mechanism (migration 0150) exists so that "one submission, one lead" holds. The
# value that keys that claim is a caller-supplied header, and **two functions decided what a
# caller's submission id means**:
#
#   apps/api  idempotency_key()   ->  None  when the id is longer than 128   (REFUSE)
#   module    claims::normalize() ->  first 128 characters                   (CAP)
#
# They disagreed about the *answer*, not about a constant — both wrote `128` — and the one
# that ran first won. The API's runs first (`crm_intake.rs:882` builds the `Submission`), so
# a submission id of 129 characters produced `submission_id: None`, `claims::take` was never
# reached, **no claim was ever written, and every retry of that one submission wrote another
# lead.** That is precisely the outcome the mechanism was built to prevent, and the unticked
# box "one submission, one lead, no duplicates" was ticked on gates that all used short keys.
#
# ## The unit test could not see it, and the second one *asserted* it
#
# `an_idempotency_key_is_trimmed_capped_and_optional` ended with a 129-character header and
# asserted `None` — the defect, written down as the expected answer. A test that pins a wrong
# answer is worse than no test: it converts a bug into a specification, and the next reader
# fixing the bug sees a red test and concludes they broke something.
#
# The gate is therefore a **cross-boundary** one: it asks the API's function and the module's
# function the *same* question and requires one answer. A unit test inside either crate
# cannot cross that line by construction, which is the same blindness the phone-normalization
# defect had (tick 67) and the reason only a gate finds this class.
#
# ## What it asserts, and what a green run does NOT prove
#
# It proves the two halves AGREE. It does not prove the agreement is the *right* agreement:
# the cap value itself is pinned separately by `claims.rs`'s own tests, and the choice of
# "cap, do not refuse" is argued in `normalize`'s doc comment. The negative control below is
# what stops the gate from being satisfied by both halves being wrong in the same direction.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR="${QA_CARGO_TARGET_DIR:-/dev/shm/w8-target}"
export CARGO_INCREMENTAL=0

echo "[crm-submission-id] asking the API and the module the same question"

# **The test names are FULLY QUALIFIED, and that is load-bearing rather than stylistic.**
#
# This gate's first version passed the short names with `--exact`, and cargo answered
# "0 passed; 325 filtered out" for both — exit code 0, no output, green. A filtered-to-nothing
# run is indistinguishable from a passing one to anything that reads an exit code, which is
# the silent-pass shape this branch's own header says it exists to prevent. `--exact` matches
# the whole path, so a short name matches nothing and matches nothing *successfully*.
#
# The guard below is therefore not decoration: it fails when a name is run to zero tests, and
# it is the only thing standing between a renamed test and a gate that measures nothing. The
# negative control below it is the other half — a gate whose only test can be renamed away
# is a gate that reports on a test that no longer exists.
run_named() {
  local name="$1"
  local out status
  # **The status is captured separately from the output, because `set -e` kills the script
  # inside a command substitution before anything is printed.** The first version of this
  # gate did `out="$(cargo test ...)"` and the negative control produced a two-line log —
  # the FAIL header and the exit code, with every test name and count lost — so the proof
  # that the gate fails for the *right* reason was unreadable on the one run that mattered
  # most. `|| status=$?` keeps the output and the status both.
  status=0
  out="$(cargo test -p omnion-api --lib -- --exact --nocapture "$name" 2>&1)" || status=$?
  echo "$out" | tail -12
  if [ "$status" -ne 0 ]; then
    echo "  FAIL: '$name' did not pass (exit $status)." >&2
    return 1
  fi
  if ! grep -qE 'test result: ok\. [1-9][0-9]* passed' <<<"$out"; then
    echo "  FAIL: '$name' ran ZERO tests — a renamed or misspelled name reads as green." >&2
    echo "        Require a non-zero 'passed' count, or the gate measures nothing." >&2
    return 1
  fi
}

run_named routes::crm_intake::tests::the_api_and_the_module_agree_on_every_submission_id
run_named routes::crm_intake::tests::an_absent_or_blank_header_is_no_identity_on_both_halves
# The one whose assertion WAS the defect: it used to read `None` for a 129-character key.
run_named routes::crm_intake::tests::an_idempotency_key_is_trimmed_capped_and_optional

# The module's own half, so "agree" cannot be satisfied by two identical refusals.
echo "[crm-submission-id] the module's own claims tests"
MODULE_OUT="$(cargo test -p omnion-module-crm-intake --lib claims:: -- --nocapture 2>&1)"
echo "$MODULE_OUT" | tail -12
if ! grep -qE 'test result: ok\. [1-9][0-9]* passed' <<<"$MODULE_OUT"; then
  echo "  FAIL: the module's claims tests ran ZERO tests." >&2
  exit 1
fi

# The durable consequence, which is the sentence the acceptance box actually promises:
# a long-keyed submission, sent twice, is ONE lead. This is a database gate because the
# claim is a row and no pure function can see whether it was written.
if [ -n "${QA_CLAIMS_DB:-}" ]; then
  echo "[crm-submission-id] durable row check against ${QA_CLAIMS_DB}"
  QA_DB="${QA_CLAIMS_DB}" bash scripts/qa/run-crm-claims.sh
else
  echo "[crm-submission-id] QA_CLAIMS_DB unset — the cross-boundary half ran, the row half did not."
  echo "                   Set QA_CLAIMS_DB=omnion_qa_w8_subid to include it."
fi
