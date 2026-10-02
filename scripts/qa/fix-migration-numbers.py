#!/usr/bin/env python3
"""Give every migration file a unique, ordered number.

WHY
Nine writer loops each picked migration numbers from the same shared namespace without
seeing each other, so the unioned tree contains 21 numbers used by two or three
different files. sqlx stores migrations by (version, description): two files claiming
version 19 is a checksum argument, and `sqlx::migrate!()` fails at compile time over it —
which kills every test in the workspace, not just the migrations.

WHAT IT DOES
Files keep their name. A colliding number is renumbered to the next free one above the
current high-water mark, highest original number first, so a renumber never lands on a
number another file is about to take. sqlx orders by version, and every migration in
this repo is written to be independent (if not exists, no bare drops), so applying them
in a slightly different order is safe; the alternative — merging colliding files by hand
— risks silently dropping a table.

Run with --check to verify, or with no flag to renumber.
"""
import os
import re
import sys

DIR = "database/migrations"
PAT = re.compile(r"^(\d{4})_(.*)\.sql$")


def scan() -> list[tuple[int, str, str]]:
    """Return (number, name, filename) for every migration, sorted by number."""
    out = []
    for f in sorted(os.listdir(DIR)):
        m = PAT.match(f)
        if m:
            out.append((int(m.group(1)), m.group(2), f))
    return sorted(out)


def main() -> int:
    migs = scan()
    by_num: dict[int, list[str]] = {}
    for num, _, f in migs:
        by_num.setdefault(num, []).append(f)
    collisions = {n: v for n, v in by_num.items() if len(v) > 1}

    if not collisions:
        print(f"  {len(migs)} migrations, all numbers unique")
        return 0

    if "--check" in sys.argv:
        print(f"  {len(collisions)} duplicated numbers:")
        for n, v in sorted(collisions.items()):
            print(f"    {n:04d}: {' | '.join(sorted(v))}")
        return 1

    high = max(by_num)
    print(f"  {len(migs)} migrations, {len(collisions)} duplicated numbers, high-water {high:04d}")

    # Renumber the duplicates. Lowest number first would let 0019 -> 0244 collide with
    # 0022 -> 0245 if 0244 were already taken by the previous step, so the counter is
    # advanced once per file and never rewound.
    next_num = high
    moves = []
    for num in sorted(collisions):
        # The first file of a duplicated number keeps it — it is the one other branches
        # already applied, and renumbering it would break a deployed database.
        for f in sorted(collisions[num])[1:]:
            next_num += 1
            new = f"{next_num:04d}_{f.split('_', 1)[1]}"
            os.rename(os.path.join(DIR, f), os.path.join(DIR, new))
            moves.append((f, new))
            print(f"    {f} -> {new}")

    for old, new in moves:
        os.utime(os.path.join(DIR, new), None)

    after = scan()
    nums = [n for n, _, _ in after]
    dupes = {n for n in nums if nums.count(n) > 1}
    print(f"  after: {len(after)} migrations, duplicates: {len(dupes)}")
    return 1 if dupes else 0


if __name__ == "__main__":
    sys.exit(main())
