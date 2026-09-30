#!/usr/bin/env python3
"""Merge two append-only BUILD-LOG halves.

    keep every base entry (repaired)  ->  append each side's new entries.

The first design diffed the two sides to find "where each side appended" and spliced
at those offsets. Four attempts, four ways that was wrong:

1. Line-level diffing is catastrophic. SequenceMatcher matches the tail of one side
   against an unrelated region of the other, and the base between them vanishes -
   444 lines on attempt 1.
2. Block-level diffing is better but still wrong, for the reason that cost attempt 3:
   **a heading is not a unique key.** Two writers can title two different entries
   "REQ-016 slice 2". Keyed by heading, one whole body is silently dropped. The log
   is append-only, so *every* new entry belongs at the END, whatever its title.
3. The verification is what saved it each time, and its shape matters. A line COUNT
   passes while whole entries are gone. What an append-only log must actually satisfy:
     (a) no base entry lost, (b) base order unchanged, (c) every entry a side has and
     base lacks is present verbatim, (d) no entry lost a PROSE line it had.
4. A body that is a strict SUBSET of base's is damage, not an edit - an earlier
   line-level merge on this branch ate part of an entry. Repair by taking the superset.

Split entries on '# ' and '## ' only: every entry ends in a '### Lessons' subsection,
and treating those as entries glued unrelated bodies together under one heading.
"""
import subprocess
import sys
from collections import Counter

PATH = "docs/BUILD-LOG.md"


def stage(n):
    return subprocess.run(
        ["git", "show", f":{n}:{PATH}"], capture_output=True, text=True, check=True
    ).stdout


def blocks(text):
    out, cur = [], []
    for line in text.splitlines(keepends=True):
        if (line.startswith("# ") or line.startswith("## ")) and cur:
            out.append(tuple(cur))
            cur = [line]
        else:
            cur.append(line)
    if cur:
        out.append(tuple(cur))
    return out


def head(b):
    return b[0].strip() if b and b[0].startswith("#") else "(preamble)"


def content(b):
    """Prose lines: a blank is not history, and neither is a `---` rule."""
    return [l for l in b
            if l.strip() and l.strip() not in ("---", "***", "___", "===")]


def contains(big, small):
    it = iter(big)
    return all(any(x == s for x in it) for s in small)


base_t, ours_t, theirs_t = stage(1), stage(2), stage(3)
base, ours, theirs = blocks(base_t), blocks(ours_t), blocks(theirs_t)
print(f"entries base={len(base)} ours={len(ours)} theirs={len(theirs)}")

# 1. Repair base entries a side has superseded with a strict superset of the same
#    heading. A strict subset is a truncation an earlier merge caused.
index = {}
for side, arr in (("ours", ours), ("theirs(main)", theirs)):
    for b in arr:
        index.setdefault(head(b), []).append((side, b))

repaired, diverged = [], []
resolved = []
for b in base:
    cands = [c for _, c in index.get(head(b), [])]
    best = b
    for c in cands:
        if contains(best, c) and len(c) > len(best):
            best = c
    if best != b:
        side = next(s for s, c in index[head(b)] if c == best)
        repaired.append((head(b)[:60], len(b), len(best), side))
    elif cands and not all(contains(c, b) for c in cands):
        diverged.append(head(b)[:70])
    resolved.append(best)

for h, a, c, side in repaired:
    print(f"  REPAIRED {a:>4} -> {c:<4} lines  {h!r}  (from {side})")
for h in diverged:
    print(f"  DIVERGED (base kept, human should look): {h!r}")

# 2. Append whatever a side has and base lacks, by CONTENT and not by heading: two
#    entries may share a title, and both are history.
#    Each side gets its OWN copy of the base multiset. Sharing one depleted counter
#    made ours' 67 matches consume main's slots, and main's 70 base entries came out
#    "new" - which is how a 6783-line log became 11335 lines.
#    A side can carry a TRUNCATED copy of an entry base has in full -- an earlier
#    line-level merge ate part of it, and the damage rode along ever since. Step 1
#    repairs the base entry from the side holding the superset, but the damaged copy
#    then fails to match `resolved` by content, is classified "new", and is appended
#    as if it were another writer's tick. The log grows, the entry exists twice, and
#    the truncation detector at the bottom fires on the damage *this script* just
#    wrote. A block whose heading is a base heading and whose prose is a strict subset
#    of that base entry is not history; it is a wound.
merged = list(resolved)
appended = []
base_by_head = {}
for b in base:
    base_by_head.setdefault(head(b), []).append(b)


def is_damaged_copy(b):
    if head(b) not in base_by_head:
        return False
    return any(contains(c, b) and content(c) != content(b)
               for c in base_by_head[head(b)])


for side, arr in (("ours", ours), ("theirs(main)", theirs)):
    have = Counter(resolved)
    fresh, dropped = [], []
    for b in arr:
        if have[b] > 0:
            have[b] -= 1
        elif is_damaged_copy(b):
            dropped.append(head(b)[:70])
        else:
            fresh.append(b)
    merged.extend(fresh)
    appended.extend(fresh)
    print(f"  {side}: {len(fresh)} new entries appended at the end"
          + (f", {len(dropped)} damaged copies dropped" if dropped else ""))
    for h in dropped:
        print(f"    DROPPED truncated copy of an entry base holds in full: {h!r}")

# (a)+(b) no base entry lost, base order unchanged.
hb = [head(b) for b in base]
hm = [head(b) for b in merged]
it = iter(hm)
lost = [h for h in hb if not any(x == h for x in it)]
if lost:
    print("BASE ENTRIES LOST OR REORDERED:", lost[:5])
    sys.exit(1)

# (c) every entry a side has and base lacks is present verbatim -- except a damaged
#     copy, which step 2 deliberately dropped. Dropping is only allowed where the
#     merged file still holds that entry in FULL: otherwise "dropped" would be a
#     quieter way to lose history, and this check is the one that catches it.
for side, arr in (("ours", ours), ("theirs(main)", theirs)):
    for b in arr:
        if any(b == x for x in base) or b in merged:
            continue
        if is_damaged_copy(b) and any(
                head(x) == head(b) and contains(content(x), content(b))
                for x in merged if head(x) == head(b)):
            continue
        print(f"APPENDED ENTRY LOST [{side}]: {head(b)[:70]!r}")
        sys.exit(1)

# (d) truncation detector, on prose.
bm = {head(b): b for b in base}
trunc = [(head(b)[:60], len(content(bm[head(b)])), len(content(b)))
         for b in merged if head(b) in bm and len(content(b)) < len(content(bm[head(b)]))]
if trunc:
    print("ENTRIES LOST PROSE LINES:", trunc[:5])
    sys.exit(1)

text = "".join("".join(b) for b in merged)
# Markers must be on a line of their OWN: the log QUOTES them inside a sentence,
# because an earlier tick's entry documents this very bug.
for bad in ("<<<<<<<", ">>>>>>>"):
    stray = [l for l in text.splitlines() if l.strip().startswith(bad)]
    if stray:
        print("CONFLICT MARKER SURVIVED:", stray[:3])
        sys.exit(1)

# Nothing from either side may be absent. This is a SUBSET check per side, not a
# multiset of the two sides summed: both sides carry the shared base entries, so
# summing wants each shared line twice and reports the merged file - which has it once
# - as missing. That false alarm is what kept the previous run from writing.
# The damaged copies step 2 dropped are subtracted first, by the same predicate that
# dropped them -- a third copy of the rule is how it drifted out of sync with the two
# above in the first place.
for side, arr in (("ours", ours), ("theirs(main)", theirs)):
    arr = [b for b in arr if b in merged or not is_damaged_copy(b)]
    missing = Counter(arr) - Counter(merged)
    if missing:
        for line, n in list(missing.items())[:8]:
            print(f"MISSING FROM {side}", n, repr("".join(line)[:95]))
        sys.exit(1)
    print(f"  {side}: every entry line present in the merge")

with open(PATH, "w") as f:
    f.write(text)
print(f"OK {len(hm)} entries | base intact+ordered | nothing truncated | "
      f"no line from either side missing | {len(repaired)} repaired | {len(diverged)} diverged")
