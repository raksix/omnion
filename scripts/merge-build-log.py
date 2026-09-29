#!/usr/bin/env python3
"""Merge docs/BUILD-LOG.md, which is append-only at BOTH ends.

    usage: merge-build-log.py [ours] [theirs]      (default: HEAD origin/main)

Two writers touch this file and they do not touch the same end: the main
writer appends its entry at the tail, and a wave writer prepends its entry
just under the first line. `git merge` therefore lands both blocks in a
conflict every single time, and resolving it by hand has already silently
dropped whole entries on this branch.

The two refs are arguments rather than constants so the script can be tested
against a synthetic repository — see `scripts/qa/test-merge-build-log.py`. A
merge helper whose only exercise is a live conflict is a helper whose first
real use is also its first test.

The merge is: take the merge base, then splice every side's inserted run back
at the base position it was inserted at, walking the base with a cursor rather
than doing index arithmetic into a list that is being grown — an insert of 713
lines shifts every later base index, so the tail run would land 713 lines too
early. That was the first version's bug, and the reconstruction check below is
what caught it.

Verification, and the reason it is written the way it is:

  * **Splicing a side's own runs back must reproduce that side byte for byte.**
    That is the exact check, and it is not fooled by the log's repeated lines.
    An "is this an in-order subsequence" test was tried first and is wrong here:
    a greedy scan mis-aligns on the fourth `**Proof.**` and reports a perfectly
    correct merge as broken. Reconstructing the side is both stronger and
    cheaper to reason about.
  * **No base line lost, and no side's line lost** — multiset, both directions.
  * **Nothing invented** — every line in the merge belongs to the base or to
    one of the two sides, so a mis-splice cannot smuggle a line in.

A length check on its own passes a merge that duplicated one block and dropped
another of equal size, which is the failure this log has already suffered on
this branch.
"""
import subprocess
import sys
from collections import Counter
from difflib import SequenceMatcher

PATH = "docs/BUILD-LOG.md"
QUIET = 3


def show_file(spec):
    return subprocess.run(
        ["git", "show", f"{spec}:{PATH}"], capture_output=True, text=True, check=True
    ).stdout.splitlines(keepends=True)


def runs(base, side):
    """Inserted runs of `side` against `base`, as (base_end_index, lines)."""
    sm = SequenceMatcher(None, base, side, autojunk=False)
    out = []
    for tag, i1, i2, j1, j2 in sm.get_opcodes():
        if tag in ("insert", "replace") and j2 > j1:
            out.append((i2, side[j1:j2]))
    return out




OURS = sys.argv[1] if len(sys.argv) > 1 else "HEAD"
THEIRS = sys.argv[2] if len(sys.argv) > 2 else "origin/main"

mb = subprocess.run(
    ["git", "merge-base", OURS, THEIRS],
    capture_output=True, text=True, check=True,
).stdout.strip()
base, ours, theirs = show_file(mb), show_file(OURS), show_file(THEIRS)

all_runs = [(p, b, "ours") for p, b in runs(base, ours)] + \
           [(p, b, "theirs") for p, b in runs(base, theirs)]
print(f"base={len(base)} ours={len(ours)} theirs={len(theirs)}")
for pos, block, side in sorted(all_runs, key=lambda r: r[0]):
    print(f"  {side:6s} {len(block):4d} lines at base[{pos}] "
          f"· {block[0].strip()[:64] or '(blank)'}")
if len(all_runs) > QUIET:
    print(f"  … {len(all_runs) - QUIET} more run(s)")

def splice(base, to_apply):
    """Splice runs into base by WALKING it, never by index arithmetic.

    Inserting at base index 53 into a list that then grows shifts every later
    index, so the second run's base index 3259 lands 713 lines too early. The
    first version of this function did that, and the reconstruction check
    below is what caught it. Walking a cursor and emitting at it cannot drift.
    """
    at = {}
    for pos, block in to_apply:
        at.setdefault(pos, []).append(block)
    out = []
    for i, line in enumerate(base):
        out.extend(l for block in at.get(i, ()) for l in block)
        out.append(line)
    out.extend(l for block in at.get(len(base), ()) for l in block)
    return out


# One cursor-walk, so positions never drift. Two runs at the same index go in
# in the order the caller supplies them, which keeps the result deterministic.
merged = splice(base, [(p, b) for p, b, _s in sorted(all_runs, key=lambda r: (r[0], r[2]))])

problems = []
expected = len(base) + sum(len(b) for _, b, _ in all_runs)
if len(merged) != expected:
    problems.append(f"length {len(merged)} != base+inserts {expected}")

# The exact check: splicing a side's own runs back must REPRODUCE that side
# byte for byte. That is stronger than "is an in-order subsequence" and it is
# not fooled by the log's repeated lines — a greedy subsequence scan
# mis-aligns on the fourth `**Proof.**` and reports a correct merge as broken.
for label, side_lines in (("ours", ours), ("theirs", theirs)):
    if splice(base, [(p, b) for p, b, _s in all_runs if _s == label]) != side_lines:
        problems.append(f"{label} is not reproduced by splicing its own runs back")

# No base line lost, and no side's line lost.
c_merged, c_base = Counter(merged), Counter(base)
for line, n in (c_base - c_merged).items():
    problems.append(f"base line lost x{n}: {line[:90]!r}")
for label, small in (("ours", Counter(ours)), ("theirs", Counter(theirs))):
    for line, n in (small - c_merged).items():
        problems.append(f"{label} line lost x{n}: {line[:90]!r}")

# And nothing invented: every line in the merge comes from some side.
c_ours, c_theirs = Counter(ours), Counter(theirs)
for line, n in (c_merged - c_base - c_ours - c_theirs).items():
    problems.append(f"line belongs to no side: {line[:90]!r} x{n}")

if problems:
    print("\n".join(f"FAIL: {p}" for p in problems[:QUIET]))
    print(f"({len(problems)} problem(s))")
    sys.exit("MERGE VERIFICATION FAILED")

with open(PATH, "w", encoding="utf-8") as fh:
    fh.write("".join(merged))
print(f"OK {PATH}: {len(base)} + {expected - len(base)} = {len(merged)} lines; "
      f"both sides' prose survives in order, no base line lost")
