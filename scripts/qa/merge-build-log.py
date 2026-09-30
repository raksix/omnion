#!/usr/bin/env python3
"""Three-way merge for an append-only log (every side only ADDS at the tail).

base/ours/theirs are the three blob contents. Insert opcodes from base->ours and
base->theirs are spliced into base; anything else (a deletion, a replacement) is
a violation of append-only and is reported rather than silently applied.

VERIFICATION. The obvious check — `(Counter(ours) + Counter(theirs)) -
Counter(merged)` — is wrong, and it fails every time it is run: the two sides
share every line they inherited from base, so their counts SUM to twice what the
merged file can hold. A build log is ~600 entries and ~40% of its lines are blank,
so the failure always reads as "MISSING x372 ''" — a log full of missing blank
lines, which is nonsense and easy to talk yourself out of.

The identity that actually holds, because the merge only splices inserts into
base, is

    Counter(merged) == Counter(base) + Counter(inserted_ours) + Counter(inserted_theirs)

checked for equality, not for containment. It catches a dropped block, a block
spliced in twice, and a block spliced at the wrong offset (which changes the
counter for that block even when every line is present). Line *counts* alone are
not a check: base+ours+theirs=total passes while half a block is duplicated.
"""
import difflib
import subprocess
import sys
from collections import Counter


def rev(*specs):
    return subprocess.run(["git", "show", *specs], capture_output=True, text=True, check=True).stdout


def ops_inserts(base, other):
    """Return [(start, end, inserted_lines)] for pure inserts, plus any non-insert edit."""
    sm = difflib.SequenceMatcher(None, base, other, autojunk=False)
    ins, bad = [], []
    for tag, i1, i2, j1, j2 in sm.get_opcodes():
        if tag == "insert":
            ins.append((i1, i2, other[j1:j2]))
        elif tag != "equal":
            bad.append((tag, i1, i2, j1, j2, other[j1:j2][:3]))
    return ins, bad


def strip_trailing_blank(lines):
    if lines and lines[-1] == "":
        lines.pop()
    return lines


def main():
    path, base_spec, ours_spec, theirs_spec = sys.argv[1:5]
    base = strip_trailing_blank(rev(base_spec).split("\n"))
    ours = strip_trailing_blank(rev(ours_spec).split("\n"))
    theirs = strip_trailing_blank(rev(theirs_spec).split("\n"))

    ours_ins, ours_bad = ops_inserts(base, ours)
    theirs_ins, theirs_bad = ops_inserts(base, theirs)

    if ours_bad or theirs_bad:
        print("NON-APPEND-ONLY EDITS DETECTED — resolve by hand")
        for side, bad in (("ours", ours_bad), ("theirs", theirs_bad)):
            for b in bad:
                print(" ", side, b)
        sys.exit(2)

    merged = list(base)
    # Splice from the tail backwards so earlier offsets stay valid.
    splices = sorted(
        [(i1, i2, blk, "ours") for i1, i2, blk in ours_ins]
        + [(i1, i2, blk, "theirs") for i1, i2, blk in theirs_ins],
        key=lambda s: (-s[0], s[3] == "theirs"),
    )
    for i1, i2, blk, _side in splices:
        merged[i1:i2] = blk

    expected = Counter(base)
    for blk in [b for _, _, b in ours_ins] + [b for _, _, b in theirs_ins]:
        expected.update(blk)
    got = Counter(merged)
    if got != expected:
        print("MERGE VERIFICATION FAILED")
        for k, v in list((expected - got).items())[:20]:
            print("  MISSING x%d %r" % (v, k[:90]))
        for k, v in list((got - expected).items())[:20]:
            print("  UNEXPECTED x%d %r" % (v, k[:90]))
        sys.exit(3)

    out = "\n".join(merged) + "\n"
    with open(path, "w") as fh:
        fh.write(out)
    print(
        "merged %s: base=%d ours=%d theirs=%d -> merged=%d (+%d) | exact multiset OK"
        % (path, len(base), len(ours), len(theirs), len(merged), len(merged) - len(base))
    )


if __name__ == "__main__":
    main()
