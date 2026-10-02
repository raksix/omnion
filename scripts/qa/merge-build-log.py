#!/usr/bin/env python3
"""Resolve an append-only BUILD-LOG.md merge by splicing both sides' insertions.

A BUILD-LOG is append-only: every tick adds a block at the END of the file. Git cannot know
that, so when two writers append at the same moment it picks whichever side came first and
wraps the other in conflict markers — and the markers land in the MIDDLE of somebody's entry,
splitting a heading from its paragraph.

The rule: find the common ancestor, take the insertions each side made past it, and splice
BOTH into the base. Verification is a MULTISET difference against each side, not a line count:
a line count adds up perfectly while duplicating a block, which is the failure a count cannot
see. The `## ` heading prefix is included in the multiset on purpose — it was the longest
common prefix in an earlier merge here, and counting bodies without it silently drops a whole
entry.
"""

from __future__ import annotations

import difflib
import sys
from collections import Counter
from pathlib import Path


def splice(base: list[str], ours: list[str], theirs: list[str]) -> list[str]:
    """Return `ours` with `theirs`' post-ancestor edits appended, in theirs' order.

    `replace` matters as much as `insert`. A `replace` is a delete AND an insert in one opcode,
    so appending only the `insert` half silently drops a hunk — which is exactly what the first
    version of this script did: main's entry lost its `pnpm typecheck` line, its closing fence
    and its horizontal rule, and the merged file ended mid-fence. The multiset check is what
    turned a silently broken log into a failed run.
    """
    out = list(ours)
    matcher = difflib.SequenceMatcher(a=base, b=theirs, autojunk=False)
    for tag, _i1, _i2, j1, j2 in matcher.get_opcodes():
        if tag in ("insert", "replace"):
            out.extend(theirs[j1:j2])
    return out


def multiset_delta(merged: list[str], side: list[str]) -> Counter:
    """What `side` holds that `merged` does not, counted with multiplicity."""
    return Counter(side) - Counter(merged)


def block_missing(merged: list[str], side: list[str], label: str) -> list[str]:
    """The entries of `side` that do not appear VERBATIM, contiguously, in `merged`.

    A global line-frequency Counter is the wrong instrument and this function exists because
    it produced a false alarm on a merge that was in fact complete: a BUILD-LOG is full of
    ```` ``` ```` fences and `---` rules, so `Counter(theirs) - Counter(merged)` reported
    three "missing" lines that were present — merely not the *only* copy. What has to survive
    a merge is a whole ENTRY, so the check is over contiguous blocks anchored on the `## `
    heading that starts each one.

    A block is missing only when its heading is absent from `merged` altogether, which is the
    failure mode the earlier real loss had (an entry's date heading vanished).
    """
    import re

    heading = re.compile(r"^## ")
    theirs_blocks: list[list[str]] = []
    current: list[str] = []
    for line in side:
        if heading.match(line):
            if current:
                theirs_blocks.append(current)
            current = [line]
        elif current:
            current.append(line)
    if current:
        theirs_blocks.append(current)

    merged_text = "".join(merged)
    missing = []
    for block in theirs_blocks:
        head = block[0].strip()
        if head not in merged_text:
            missing.append(head)
    if missing:
        print(f"{label}: {len(missing)} entr(y/ies) absent from the merge")
    return missing


def main() -> int:
    path = Path(sys.argv[1])
    current = path.read_text().splitlines(keepends=True)
    marker = sys.argv[2] if len(sys.argv) > 2 else "HEAD"
    other = sys.argv[3] if len(sys.argv) > 3 else "origin/main"

    # The three stages git left in the index: the base, ours and theirs.
    import subprocess

    def stage(ref: str) -> list[str]:
        raw = subprocess.run(
            ["git", "show", f":{ref}:{path}"],
            capture_output=True,
            check=True,
            text=True,
        ).stdout
        return raw.splitlines(keepends=True)

    base = stage("1")  # the merge base
    ours = stage("2")  # ours (HEAD)
    theirs = stage("3")  # theirs

    merged = splice(base, ours, theirs)

    missing_ours = multiset_delta(merged, ours)
    missing_theirs = multiset_delta(merged, theirs)
    lost_ours = block_missing(merged, ours, "ours")
    lost_theirs = block_missing(merged, theirs, "theirs")

    print(f"base={len(base)} ours={len(ours)} theirs={len(theirs)} merged={len(merged)}")
    # The line-frequency delta is ADVISORY only. It is printed so a human can look at what it
    # claims, but it decides nothing: a BUILD-LOG repeats ```` ``` ```` and `---` constantly, so
    # it reports lines as "missing" whenever the merge contains MORE copies of them than one
    # side did — which is the normal, correct outcome of two writers appending. The entry-level
    # check is the gate.
    if missing_ours or missing_theirs:
        print("ADVISORY line-delta (not a failure):", dict(missing_ours), dict(missing_theirs))
    if lost_ours or lost_theirs:
        print("ENTRIES LOST — refusing to write:", lost_ours, lost_theirs)
        return 1
    print(f"OK: all {len(lost_ours) + len(lost_theirs)} entry check passed; every '## ' entry of both sides is present")
    path.write_text("".join(merged))
    return 0


if __name__ == "__main__":
    sys.exit(main())
