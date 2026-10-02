#!/usr/bin/env python3
"""Dead-export detector for `apps/admin` (TypeScript).

`scripts/qa/dead-callers.py` does this for Rust. The same branch pays the same
defect in a second language, so the tool came with it -- and it carried the same
kind of bug on its first run, which is why the false-positive history is written
into the body rather than the commit message:

  1. **`Path` compared to `str`.** The definition's own file was compared against
     a string path, so the comparison was always unequal, the "mentioned in no
     other file" set was always empty, and the first honest run reported **zero**
     candidates over 318 exported symbols. A detector that reports nothing is
     either a clean codebase or a broken tool, and on this branch the answer is
     almost always the second one.

  2. **Template-literal bodies blanked.** `strip_noise` erased string and template
     *contents*, which is right for a Rust sweep (a doc link is not a call) and
     wrong here: this codebase calls its own functions from inside JSX template
     literals -- `className={`... ${scanTone(status)}`}` -- and every one of those
     read as uncalled. A screen's *styling* is exactly where a dead helper hides.

  3. **Next route entries counted as dead.** 68 of the 77 first-pass candidates
     were `app/**/page.tsx` default exports, which the framework calls by
     convention. They are reported separately, never mixed with real findings.

A symbol is dead when no line outside its own definition mentions it. The Next
route entries are excluded by name; everything else needs hand triage, and a
triaged finding is a *defect* only after reading the consumer.

Usage:  python3 scripts/qa/dead-exports.py [--json] [--admin apps/admin]
Exit code is always 0: a report, not a gate.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from collections import defaultdict
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]

DEF_FN = re.compile(
    r"^\s*export\s+(?:default\s+)?(?:async\s+)?function\s+([A-Za-z_$][\w$]*)")
DEF_CLASS = re.compile(
    r"^\s*export\s+(?:default\s+)?(?:abstract\s+)?class\s+([A-Za-z_$][\w$]*)")
DEF_CONST = re.compile(
    r"^\s*export\s+(?:declare\s+)?const\s+([A-Za-z_$][\w$]*)\s*(?::[^=]*)?=")
DEF_REEXPORT = re.compile(r"^\s*export\s*\{([^}]*)\}")

WORD = re.compile(r"[A-Za-z_$][\w$]*")

# Next.js / React call the file's own default export by convention.
FRAMEWORK_CALLED = {
    "default", "metadata", "generateMetadata", "generateStaticParams", "dynamic",
    "dynamicParams", "revalidate", "fetchCache", "runtime", "maxDuration",
    "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "config",
    "generateViewport", "viewport", "middleware", "instrumentation",
}
ROUTE_ENTRY_NAMES = {"page.tsx", "layout.tsx", "route.ts", "template.tsx"}

# `groups.some((group) => group.status === "pending")` -- a function passed as a
# value to `.filter`/`.map` has no call parentheses, so a call-shaped matcher
# alone reads it as uncalled.
VALUE_USE = re.compile(
    r"[({[,=]\s*(?:[A-Za-z_$][\w$]*\.)*([A-Za-z_$][\w$]*)\s*[,)}\]]")


def strip_comments(src: str) -> str:
    """Blank `//` and `/* */`, preserving newlines and offsets.

    String and template *bodies* are deliberately left alone. The Rust sweep
    strips them because a doc link is not a call site; in this codebase a
    template literal is where code is *executed*, and blanking it made every
    helper used from JSX look dead. A name that occurs only inside a string is a
    rare false negative here, and a false negative is the safe direction for a
    report that is hand-triaged.
    """
    out = []
    i, n = 0, len(src)
    while i < n:
        two = src[i : i + 2]
        if two == "//":
            j = src.find("\n", i)
            i = n if j < 0 else j
            out.append(" ")
            continue
        if two == "/*":
            depth, j = 1, i + 2
            while j < n and depth:
                if src[j : j + 2] == "/*":
                    depth += 1
                    j += 2
                elif src[j : j + 2] == "*/":
                    depth -= 1
                    j += 2
                else:
                    j += 1
            out.append("".join(c if c == "\n" else " " for c in src[i:j]))
            i = j
            continue
        out.append(src[i])
        i += 1
    return "".join(out)


def sources(admin: Path) -> dict[Path, str]:
    return {
        f: strip_comments(f.read_text(encoding="utf-8", errors="replace"))
        for f in sorted(admin.rglob("*"))
        if f.suffix in (".ts", ".tsx")
        and "node_modules" not in f.parts
        and not f.name.endswith(".d.ts")
    }


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--admin", default="apps/admin")
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args()
    admin = (REPO / args.admin).resolve()
    cache = sources(admin)

    defs: dict[str, list[tuple[Path, int]]] = defaultdict(list)
    for f, src in cache.items():
        for idx, line in enumerate(src.split("\n")):
            for rx in (DEF_FN, DEF_CLASS, DEF_CONST):
                m = rx.match(line)
                if m:
                    defs[m.group(1)].append((f, idx + 1))
            m = DEF_REEXPORT.match(line)
            if m:
                for part in m.group(1).split(","):
                    nm = part.split(" as ")[-1].strip()
                    if re.fullmatch(r"[A-Za-z_$][\w$]*", nm):
                        defs[nm].append((f, idx + 1))

    # Every line that MENTIONS the name. `Path` keys throughout -- the first
    # version compared a Path against a str and reported nothing at all.
    mentions: dict[str, list[tuple[Path, int]]] = defaultdict(list)
    for f, src in cache.items():
        for idx, line in enumerate(src.split("\n")):
            for w in set(WORD.findall(line)):
                mentions[w].append((f, idx + 1))
            for m in VALUE_USE.finditer(line):
                mentions[m.group(1)].append((f, idx + 1))

    findings, route_entries = [], []
    for name, sites in sorted(defs.items()):
        if name in FRAMEWORK_CALLED:
            continue
        dset = set(sites)
        refs = [s for s in mentions.get(name, []) if s not in dset]
        if refs:
            continue
        is_route = all(
            f.name in ROUTE_ENTRY_NAMES and "app" in f.parts for f, _ in sites
        )
        (route_entries if is_route else findings).append(
            {"name": name, "definitions": [f"{f.relative_to(admin)}:{ln}" for f, ln in sites]}
        )

    if args.json:
        print(json.dumps({"dead": findings, "route_entries": len(route_entries)}, indent=2))
        return 0

    print(f"dead-export scan ({args.admin}): {len(defs)} exported symbols, "
          f"{len(cache)} files")
    print(f"  called by the framework by convention (Next route entries): {len(route_entries)}")
    print(f"  no reference outside their own definition: {len(findings)}")
    if not findings:
        print("\nnothing to triage -- which, on this branch, means check the tool first")
        return 0
    print("\nhand-triage each; a triage that reads as 'harmless' is the finding:\n")
    for fd in findings:
        print(f"  {fd['name']:<34} {', '.join(fd['definitions'][:3])}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
