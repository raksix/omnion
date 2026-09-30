#!/usr/bin/env python3
"""The source editor's refresh rule and its open path, against the real module.

    node --experimental-strip-types scripts/qa/crm-source-editor.ts
    python3 scripts/qa/crm-source-editor-refresh.py <repo-root>

## The defect this exists for

Two, both in `apps/admin/features/crm-intake/intake-sources.tsx`, both found by the
dead-export sweep `scripts/qa/dead-exports.py` when it flagged `fetchIntakeSource` — an
exported client function, sitting next to `createIntakeSource`/`updateIntakeSource`/
`deleteIntakeSource`/`rotateIntakeKey` (all four called), with **no reader in the module's
whole life**.

**1. The editor opened from the list row, and the save is a whole-column patch.**
`EditorState` carries nine fields and `save` sends all nine, so opening the editor from the
`rows` array captured at load time is a **lost update**: an operator who opens a source
thirty seconds after somebody else renamed it saves the *old* name back, and the API accepts
it because the payload is valid. The read endpoint for one source existed the whole time and
would have answered with the row as it is *now*; nothing was lost by routing the click
through it, and the alternative is a silent revert of a colleague's edit. This is not a
hypothetical field drift — `list_sources` and `find_source` select the same
`SOURCE_COLUMNS` today, so the two bodies are identical and the bug is *invisible until two
people are in the screen at once*, which is exactly the class of defect a static check cannot
be handed the credit for catching.

**2. The refresh handler said the opposite of what it did.** Its comment read *"the editor
only follows the server when it is closed, and when the source it is editing is gone it
closes too"* — and the code replaced an **open** editor's state with the freshly fetched row
while returning `null` for a **closed** one, which is the inverse: a half-typed mapping, a
renamed source and a consent wording in progress were all discarded by `Refresh`, the one
gesture that is a *read*. The stated intent is right and the code was its mirror.

## Why the rule is tested as a FUNCTION and not as a grep

The decision is a two-line `setState` callback, and asserting on its *text* proves only that
some words are present. So `crm-source-editor.ts` imports the real
`apps/admin/lib/crm-intake.ts` and drives `editorAfterRefresh` over every case, and this file
runs it. The negative controls matter more than the positives here: a rule whose
implementation returned "keep" unconditionally would pass a happy-path-only suite, and the
case that must **close** is the one that stops an editor writing to a deleted row.

Node's `--experimental-strip-types` is what makes this possible without a test runner: the
admin panel has no test framework (no `vitest`, no `jest`, no `*.test.ts` in `apps/admin`),
and adding one for a 20-line pure function would be a larger change than the fix. The flag
erases types and keeps the value, which is all a behaviour test of a pure function needs.

Exit code 0 = every assertion holds. Non-zero = the number of failures, one per line.
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

FAILURES: list[str] = []


def check(label: str, ok: bool, detail: str = "") -> None:
    if ok:
        print(f"  ok    {label}")
    else:
        print(f"  FAIL  {label}" + (f" — {detail}" if detail else ""))
        FAILURES.append(label)


def code_of(path: Path) -> str:
    return path.read_text(encoding="utf-8", errors="replace")


def main() -> int:
    if len(sys.argv) < 2:
        print("usage: crm-source-editor-refresh.py <repo-root>", file=sys.stderr)
        return 2
    root = Path(sys.argv[1]).resolve()
    screen = root / "apps/admin/features/crm-intake/intake-sources.tsx"
    api = root / "apps/admin/lib/crm-intake-api.ts"
    pure = root / "apps/admin/lib/crm-intake.ts"
    driver = root / "scripts/qa/crm-source-editor.ts"
    for path in (screen, api, pure, driver):
        if not path.is_file():
            print(f"  FAIL  {path.relative_to(root)} is missing")
            FAILURES.append(str(path.relative_to(root)))
    if FAILURES:
        print(f"\nFAIL: {len(FAILURES)} of {len(FAILURES)} assertion(s) failed")
        return len(FAILURES)

    text = code_of(screen)
    pure_text = code_of(pure)

    # --- 0 · the driver must actually run, or every behavioural line below is theatre ---------
    node = subprocess.run(
        ["node", "--experimental-strip-types", str(driver), str(root)],
        capture_output=True,
        text=True,
        cwd=str(root),
    )
    out = node.stdout
    check(
        "the behavioural driver runs on the real module",
        node.returncode == 0,
        f"exit {node.returncode}: {(node.stderr or '').strip()[:400]}",
    )
    for line in out.splitlines():
        print(f"    | {line}")

    def passed(name: str) -> bool:
        return f"PASS {name}" in out

    def failed(name: str) -> bool:
        return f"FAIL {name}" in out

    # The driver's own self-checks. Quoted by name so a missing case is a failure rather than
    # a silently shorter suite: a suite that stops printing because an import broke looks
    # exactly like a suite that passed.
    for case in (
        "a closed editor stays closed",
        "an open editor keeps its in-flight edits",
        "an editor whose source vanished closes",
        "another tenant's source never adopts this editor",
        "an id match on name alone never counts",
        "an empty list closes an open editor",
    ):
        check(f"rule: {case}", passed(case))
    check(
        "the rule suite has no failing case",
        not re.search(r"^FAIL ", out, re.M),
        "the driver reported a failing case — see the lines above",
    )
    # A driver that printed nothing at all would satisfy "no failing case".
    check("the driver printed its case verdicts", out.count("PASS ") >= 6, f"{out.count('PASS ')} PASS lines")

    # --- 1 · the open path reads the source, and reads it exactly once ------------------------
    # Absence plus uniqueness. `fetchIntakeSources` may appear any number of times (the list
    # legitimately needs it); what must not happen is a second call site for the *single*
    # read, because the second one is a second, drifting answer to "what does this row hold".
    single_calls = re.findall(r"\bfetchIntakeSource\(", text)
    check(
        "the single-source read has exactly one call site in the screen",
        len(single_calls) == 1,
        f"found {len(single_calls)}: a second one is a second answer to the same question",
    )
    check(
        "the open path is the one that calls it",
        re.search(r"openEditor[\s\S]{0,600}?fetchIntakeSource\(", text) is not None,
        "the read is not reached from openEditor",
    )
    check(
        "the Edit button goes through openEditor, not through the list row",
        re.search(r'data-source-edit=\{source\.id\}[\s\S]{0,400}?void openEditor\(source\.id\)', text)
        is not None,
        "the Edit button still opens the editor from the row it was handed",
    )
    # The defect, stated as the thing that must be absent.
    check(
        "no Edit handler opens the editor straight from the list row",
        re.search(r"onClick=\{\(\) => setEditing\(editorOf\(source\)\)\}", text) is None,
        "the old row-to-editor path is back: a save is a whole-column patch over a stale copy",
    )
    check(
        "the import of the single-source read is present",
        re.search(r"^\s*fetchIntakeSource,\s*$", text, re.M) is not None,
        "the screen calls it but does not import it",
    )

    # --- 2 · the refresh rule is the pure function, and only the pure function ----------------
    # A second implementation of "is this editor still real" anywhere in the screen is the
    # duplication this module's signature defect has produced before.
    inline_matches = re.findall(r"setEditing\(\(current\)\s*=>\s*\{", text)
    check(
        "the refresh is the only setEditing((current) => …) callback",
        len(inline_matches) == 1,
        f"found {len(inline_matches)}: one of them is a second copy of the rule",
    )
    check(
        "the refresh callback delegates to editorAfterRefresh",
        re.search(r"setEditing\(\(current\)\s*=>\s*\{\s*const verdict = editorAfterRefresh\(", text)
        is not None,
        "the callback computes the rule itself instead of asking",
    )
    check(
        "the refresh never re-reads the row into an open editor",
        "editorOf(fresh)" not in text,
        "a refresh still overwrites in-flight edits with the fetched row",
    )
    check(
        "the rule is exported from the pure module, not local to the screen",
        "export function editorAfterRefresh" in pure_text,
        "the rule is not in lib/crm-intake.ts, so it cannot be driven directly",
    )
    check(
        "the screen imports the rule rather than redeclaring it",
        re.search(r"^\s*editorAfterRefresh,\s*$", text, re.M) is not None,
        "the rule is used but not imported",
    )

    # --- 3 · the generic and the concrete must not drift -------------------------------------
    # The client read is a URL template; the route is a path. Nothing else checks that the
    # string in the panel is the path the API registered, and a rename on either side is a
    # 404 behind a button that looks alive.
    url = re.search(
        r"export function fetchIntakeSource\([^)]*\)[^{]*\{\s*return request<[^>]*>\(`?([^`\"']*)`?",
        code_of(api),
    )
    check("the client read is a template over the id", url is not None, "could not read the URL out of the client")
    if url is not None:
        path = url.group(1)
        check(
            "the client read targets the single-source path",
            path == "/api/v1/crm/intake/sources/${encodeURIComponent(id)}",
            f"got {path!r}",
        )
        route = root / "apps/api/src/routes/mod.rs"
        registered = re.search(r'"/crm/intake/sources/\{id\}"', code_of(route))
        check("that path is registered by the router", registered is not None, "no /crm/intake/sources/{id} route")

    # The opening row is a real loading state, not a button that does nothing while it waits:
    # a click with no visible answer reads as a dead control, which the walkthrough calls a
    # high finding and which this branch has shipped once.
    #
    # The load flag must also be *cleared in a `finally`*. A `setOpening(null)` in the `try`
    # after a successful read, and nothing on the error path, is a row that spins for ever the
    # first time the API refuses — and the refusal that leaves the screen stuck is the one an
    # operator hits when the API is already in the state they are trying to diagnose.
    check(
        "the opening row announces itself while the read is in flight",
        re.search(r"opening === source\.id", text) is not None,
        "no per-row spinner: a click with no visible answer reads as a dead control",
    )
    finally_block = re.search(
        r"const openEditor[\s\S]*?finally\s*\{\s*setOpening\(null\);\s*\}", text
    )
    check(
        "the opening flag is cleared in a finally, so a refused read cannot spin for ever",
        finally_block is not None,
        "setOpening(null) is not the body of openEditor's finally",
    )
    check(
        "the opening flag is set before the read, not after",
        re.search(r"const openEditor[\s\S]*?setOpening\(id\);[\s\S]*?await fetchIntakeSource\(", text)
        is not None,
        "the flag is not set ahead of the request, so the spinner can only ever appear late",
    )
    check(
        "a failed open is reported rather than swallowed",
        re.search(r"catch \(caught\)[\s\S]{0,200}?could not be opened", text) is not None,
        "an open failure is not surfaced",
    )

    # --- 4 · the walkthrough's new assertion names a selector the screen really renders --------
    # This check exists because the first draft of the walkthrough step asserted on
    # `[data-source-name]`, which the screen does not render: `inputValue()` returned `null`,
    # the comparison was guarded by `!== null`, and the step would have gone GREEN on a control
    # that is not there. A selector is part of the contract being asserted, so the gate reads
    # the selector out of the assertion and looks it up in the screen's own markup.
    walk = root / "scripts/qa/walkthrough.cjs"
    walk_text = code_of(walk)
    # The selector is found by the BINDING, and it must be found in the same statement as the
    # assertion. Two versions of this check were wrong before the third was right, and both were
    # wrong the same way — **a search that is not anchored to the thing it is checking reads the
    # first match in a 9,000-line file and calls it evidence**:
    #
    #   1. scanning forward from the assertion to the next `.locator(` reported
    #      `[data-required-target=job_title]`, three lines further down, which the assertion
    #      never reads;
    #   2. dropping the anchor entirely reported the file's *first* `page.locator` binding in
    #      the storefront pass (`siteId`, a different feature entirely) and failed against a
    #      perfectly correct step.
    #
    # So the region is cut out first — from the click to the end of the assertion — and both
    # facts are required to come from inside it. A third failure mode is refused explicitly:
    # if the cut region holds no binding at all, that is a failure, not a pass.
    # `re.S` is load-bearing and was missing on the first run: `.{0,400}` without it stops at
    # the first newline, and the assertion this check reads is written on the line AFTER the
    # `=`. So the body it compared against was the empty string, and a correct step failed two
    # assertions. A capture window that can silently be empty is worse than no window — it fails
    # for a reason that has nothing to do with the defect, which is precisely how a gate gets
    # ignored.
    region = re.search(
        r"data-source-edit[\s\S]{0,3000}?steps\.editorOpenedFromServer\s*=(.{0,400})",
        walk_text,
        re.S,
    )
    check(
        "the open-path step is reachable in one region of the walkthrough",
        region is not None,
        "could not find the click and its assertion within 3000 characters",
    )
    if region is not None:
        step_text = region.group(0)
        binding = re.search(
            r"const\s+(\w+)\s*=\s*await\s*page\s*\n?\s*\.locator\(\s*\"([^\"]+)\"\s*\)", step_text
        )
        check(
            "the step binds the editor's name input to a variable",
            binding is not None,
            "no `const x = await page.locator(\"…\")` between the click and the assertion",
        )
        if binding is not None:
            var_name, selector = binding.group(1), binding.group(2)
            body = region.group(1)
            check(
                f"the open-path assertion consumes the value that was read ({var_name})",
                var_name in body,
                "it reads a different variable — the selector check would be measuring another control",
            )
            check(
                "that assertion excludes a null read",
                f"{var_name} !== null" in body,
                f"a null read of {var_name} is not excluded, so a missing input reads as a pass",
            )
            # `#id` → the literal id attribute; `[data-x]` → the data attribute of the same name.
            if selector.startswith("#"):
                literal = f'id="{selector[1:]}"'
            else:
                attr = selector.strip("[]").split("=")[0]
                literal = f"{attr}="
            check(
                f"the screen renders the selector the assertion reads ({selector})",
                literal in text,
                f"no {literal} in intake-sources.tsx — the assertion would measure a missing control",
            )
    # The Edit click is async now, so a fixed timeout after it is a race that reports a product
    # failure the harness invented. `waitForSelector` is the wait that can actually fail.
    check(
        "the walkthrough waits for the editor instead of a fixed timeout after the click",
        re.search(r"data-source-edit[\s\S]{0,400}?waitForSelector\(\"\[\[data-source-editor\]\]\"", walk_text)
        is not None
        or re.search(r"data-source-edit[\s\S]{0,400}?waitForSelector\(\"\[\"?data-source-editor", walk_text)
        is not None,
        "still a fixed waitForTimeout after an async click — a slow API reads as a broken screen",
    )

    print()
    if FAILURES:
        print(f"FAIL: {len(FAILURES)} of {len(FAILURES)} assertion(s) failed")
        return len(FAILURES)
    print(
        "PASS: the editor opens from the server's own answer, a refresh keeps in-flight edits, "
        "and both are the only copies of that rule"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
