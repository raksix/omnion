#!/usr/bin/env python3
"""Derive the CRM flow-stepper's read surface and assert the stepper has exactly one source.

    python3 scripts/qa/crm-stepper-truth.py <repo-root>

## The defect this exists for

`GET /api/v1/crm/leads/flow` was registered, guarded with `crm.leads.read`, documented as *"which
steps of the documented flow this deployment can run"*, given a client function `fetchLeadFlow`,
and given a `FlowAvailability::to_module` whose own doc said it existed *"for the pure step
plan"*. **Nothing called any of them.** The stepper renders `detail.steps`, which
`GET /api/v1/crm/leads/{id}` already returns **computed** — each `Blocked` step naming the module
that would unblock it.

This module's recurring signature defect (fourteen instances, `docs/BUILD-LOG.md` tick 46) is a
thing that is *written, typed, documented, guarded and gated* while nothing consumes it. The three
green CRM gates could not see this one either, and the reason is the point:

- `run-crm-convert.sh` drives the store. The store was always right; the duplication was above it.
- `run-crm-intake.sh` walks the inbox and the detail. It read `detail.steps` and saw four steps.
- The walkthrough carried `stepperAgreesWithFlow`, which **did** fetch the endpoint — and then
  only used `flow.crm`, which is `true` on any installation where `crm_contacts` exists, so the
  branch it asserted was vacuous in exactly the case it was written for (the CRM being absent).

So a gate existed that read the dead endpoint, and the defect survived it: **a probe that answers
`null` for a broken endpoint makes the assertion that consumes it vacuously true.** That is why the
walkthrough assertion was replaced rather than deleted, and why this gate asserts on *absence plus
uniqueness* rather than on a value.

## Why the check is derived, not written down

A list of "these must not come back" is a snapshot that rots silently, and the third check below —
that the availability is built in exactly one place — is the only one that would notice somebody
adding a second `table_exists(pool, "sales_quotes")` call. Both are read out of the source.

Exit code 0 = every assertion holds. Non-zero = the count of failures, one per line.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

FAILURES: list[str] = []


def check(label: str, ok: bool, detail: str = "") -> None:
    if ok:
        print(f"  ok    {label}")
    else:
        print(f"  FAIL  {label}" + (f" — {detail}" if detail else ""))
        FAILURES.append(label)


def strip_comments(text: str) -> str:
    """Source without `//` and `/* */` comments.

    A `grep` over raw source finds `/crm/leads/flow` in the comment that *explains why the route
    is gone*, which is the correct state. The claims below are about code, so the code is what is
    read.

    The block-comment pass is **guarded by a cheap pre-test**. `re.sub(r'/\\*.*?\\*/', ...)` with
    `re.S` treats an unpaired `/*` as opening a comment that runs to end of file: one stray `/*`
    deletes eighty percent of the module, and every assertion downstream then fails for a reason
    that has nothing to do with the defect. It removed 100 KB of a 102 KB file on the first run of
    this gate, which is the same false-negative shape the gate exists to prevent.

    The pre-test is a **count**, and a count is wrong on its own: this file contains the literal
    `/* already_sent */` in a branch arm, which is a comment, so `/*` and `*/` are balanced — but
    a file whose last `/*` sits inside a string would read as unbalanced. So the substitution is
    attempted and then **verified**: if it removed more than a third of the file, the regex
    clearly ran away and the original text is used unchanged. Over-removing is always the
    dangerous direction here, and the assertions all fail for the wrong reason when it happens.
    """
    stripped = re.sub(r"/\*.*?\*/", "", text, flags=re.S)
    if len(stripped) < len(text) * 2 / 3:
        stripped = text
    return re.sub(r"//[^\n]*", "", stripped)


def main() -> int:
    root = Path(sys.argv[1] if len(sys.argv) > 1 else ".").resolve()
    routes = root / "apps/api/src/routes/mod.rs"
    intake = root / "apps/api/src/routes/crm_intake.rs"
    client = root / "apps/admin/lib/crm-intake-api.ts"
    walk = root / "scripts/qa/walkthrough.cjs"

    for path in (routes, intake, client, walk):
        if not path.exists():
            print(f"  FAIL  missing {path.relative_to(root)}")
            FAILURES.append(str(path.relative_to(root)))
    if FAILURES:
        return len(FAILURES)

    routes_code = strip_comments(routes.read_text(encoding="utf-8", errors="replace"))
    intake_code = strip_comments(intake.read_text(encoding="utf-8", errors="replace"))
    client_code = strip_comments(client.read_text(encoding="utf-8", errors="replace"))
    walk_code = strip_comments(walk.read_text(encoding="utf-8", errors="replace"))

    # ---------------------------------------------------------------- 1
    # The route is gone. Registration is the thing that made it a route at all, so that is
    # asserted first: a handler nobody registers is inert, a registered one is a live endpoint
    # answering `200` for every tenant.
    registered = bool(re.search(r'"/crm/leads/flow"', routes_code))
    check("GET /crm/leads/flow is not registered", not registered,
          "the route string is still in routes/mod.rs code (not a comment)")

    # ---------------------------------------------------------------- 2
    # All three artefacts of the dead design are gone, not just the registration. Leaving the
    # handler and the client function behind is what produces the *next* writer's "there is an
    # endpoint for this, I will use it" — the handler would still compile, still be exported and
    # still be the obvious answer to a question the codebase no longer supports.
    check("no `crm_intake::flow` handler reference", "crm_intake::flow" not in routes_code)
    check("no `pub async fn flow` handler left", not re.search(r"pub async fn flow\b", intake_code))
    check("no `FlowAvailability` type left", "FlowAvailability" not in intake_code)
    check("no `to_module` converter left", "to_module" not in intake_code)
    check("no `fetchLeadFlow` client function left", "fetchLeadFlow" not in client_code)
    check("no `LeadFlow` client type left", "LeadFlow" not in client_code)
    # The client must not be able to reach the path by string either, which is the form a later
    # writer would copy out of a browser's network tab rather than out of this file.
    check("no client request to /crm/leads/flow", "crm/leads/flow" not in client_code)
    check("no harness fetch of /crm/leads/flow", "crm/leads/flow" not in walk_code)

    # ---------------------------------------------------------------- 3
    # The stepper's truth has exactly ONE source. This is the assertion the three green gates
    # could not make: they each checked one link in a chain, and the chain had a redundant
    # branch. A second `Availability { … }` literal or a second lookup of either table is exactly
    # how the duplication came back.
    #
    # The count is of *constructions*, and the return type is not one. `-> …Availability {` ends
    # with a brace like a literal does, so matching `Availability\s*\{` counts the function's own
    # signature and reports a duplication that does not exist — which is the same class as the
    # walkthrough probe this file replaced: a check that fails for a reason the defect does not
    # cause trains the reader to ignore it. The lookahead is "this brace opens a body, not a
    # signature", i.e. a field name or the closing of a comment.
    availability_literals = len(
        re.findall(r"omnion_module_crm_intake::Availability\s*\{\s*(?:sales|commerce|\})", intake_code)
    )
    check("Availability is constructed in exactly one place", availability_literals == 1,
          f"found {availability_literals} construction(s); a second one is the duplication returning")

    for table in ("sales_quotes", "commerce_customers"):
        lookups = len(re.findall(rf'table_exists\(\s*pool\s*,\s*"{table}"\s*\)', intake_code))
        check(f"`{table}` is probed in exactly one place", lookups == 1,
              f"found {lookups} lookups; two sites can answer differently")

    # And the one builder must actually be the one the step plan consumes — otherwise the
    # uniqueness above is a property of a function nothing calls.
    check("the step plan consumes the shared builder",
          bool(re.search(r"module_availability\(\s*pool\s*\)\.await", intake_code))
          and "step_plan" in intake_code,
          "the detail handler no longer feeds the builder's answer into step_plan")

    # ---------------------------------------------------------------- 4
    # The regression: a `Blocked` step must still name its module. This is what the deleted
    # endpoint was standing in for, and it is the property an operator acts on. Asserted as a
    # source-level constant on both sides, because the walkthrough's replacement
    # (`blockedStepsNameTheirModule`) refuses to pass when it sees no blocked step at all — a
    # gate that can be satisfied by an empty page is not a gate.
    blocked_notes = re.findall(r"StepState::Blocked,\s*\n\s*note: \"([^\"]+)\"", intake_code)
    plan_src = (root / "modules/crm-intake/src/convert.rs").read_text(encoding="utf-8", errors="replace")
    notes_in_plan = re.findall(r"StepState::Blocked,\s*\n\s*note: \"([^\"]+)\"", plan_src)
    every = blocked_notes + notes_in_plan
    check("a Blocked step exists to be checked", len(every) > 0,
          "no Blocked branch found in the handler or the step plan — nothing to assert")
    check("every Blocked step names the module it waits for",
          bool(every) and all("module" in n.lower() or "REQ-" in n for n in every),
          f"a Blocked note does not name its remedy: {every}")

    # The harness keeps a check that can FAIL. An assertion that passes on an empty page is the
    # class this file was written for.
    check("the walkthrough replacement refuses an empty stepper",
          "blockedStepsNameTheirModule" in walk_code
          and "if (blocked.length === 0) return false" in walk_code,
          "the replacement assertion is absent or can pass with no blocked step")

    print()
    if FAILURES:
        print(f"FAIL: {len(FAILURES)} of {len(FAILURES)} assertion(s) failed")
        return len(FAILURES)
    print("PASS: the stepper reads one source and the removed endpoint is gone with all three of its artefacts")
    return 0


if __name__ == "__main__":
    sys.exit(main())
