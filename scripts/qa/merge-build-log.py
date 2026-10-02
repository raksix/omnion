#!/usr/bin/env python3
"""Merge an append-only BUILD-LOG across a merge conflict, and PROVE nothing was dropped.

Both sides append, so a merge that keeps only one side silently deletes the other
writer's whole tick history. Textual `git merge` gives a conflict; taking one side
"because it is bigger" is a guess.

The merge is a splice: every line the two sides INSERTED onto the common base is
re-inserted, in each side's own order, at the position it took on that side.

Verification is a MULTISET check, not a line count. `base + ours + theirs == merged`
holds for a merge that duplicated a block and dropped another; it is the count that
lies, which is exactly why a sibling writer's history can vanish under it. Instead:

  * every line of `ours` and of `theirs` must be present in `merged` at least as
    many times as it appears in that side (nothing dropped),
  * every `## ` heading of both sides must survive (a dropped tick is a lost day),
  * the base's own line multiset must still be covered (nothing rewritten).
"""
import subprocess
import sys
from collections import Counter
from difflib import SequenceMatcher

path = sys.argv[1] if len(sys.argv) > 1 else "docs/BUILD-LOG.md"


def git(*args):
    return subprocess.run(["git", *args], capture_output=True, text=True, check=True).stdout


def lines(text):
    return text.splitlines(keepends=True)


base = lines(git("show", f":1:{path}"))
ours = lines(git("show", f":2:{path}"))
theirs = lines(git("show", f":3:{path}"))


def inserted(side):
    """Lines side added relative to base, grouped into the blocks it added them in."""
    sm = SequenceMatcher(None, base, side, autojunk=False)
    out = []
    for tag, i1, i2, j1, j2 in sm.get_opcodes():
        if tag in ("insert", "replace") and j2 > j1:
            out.append((i1, side[j1:j2]))
    return out


# Splice both sides' insertions into the base, ordered by where they attach.
chunks = [(pos, blk, "ours") for pos, blk in inserted(ours)]
chunks += [(pos, blk, "theirs") for pos, blk in inserted(theirs)]
chunks.sort(key=lambda c: (c[0], 0 if c[2] == "ours" else 1))

merged = []
cursor = 0
for pos, blk, _side in chunks:
    if pos < cursor:  # overlapping edit region: keep the earlier block, skip the overlap
        pos = cursor
    merged.extend(base[cursor:pos])
    merged.extend(blk)
    cursor = pos
merged.extend(base[cursor:])

mc, oc, tc, bc = Counter(merged), Counter(ours), Counter(theirs), Counter(base)
missing = {s: sum((c - mc).values()) for s, c in (("ours", oc), ("theirs", tc), ("base", bc))}

head_base = [l for l in base if l.startswith("## ")]
head_ours = [l for l in ours if l.startswith("## ")]
head_theirs = [l for l in theirs if l.startswith("## ")]
lost_heads = [h for h in set(head_ours) | set(head_theirs) if h not in mc]

with open(path, "w", encoding="utf-8") as fh:
    fh.write("".join(merged))

print(
    f"base={len(base)} ours={len(ours)} theirs={len(theirs)} merged={len(merged)}\n"
    f"missing lines: {missing}\n"
    f"lost headings: {len(lost_heads)}"
)
for h in lost_heads[:10]:
    print("  LOST", h.strip())
if any(missing.values()) or lost_heads:
    sys.exit(1)
