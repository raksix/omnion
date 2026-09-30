#!/usr/bin/env python3
"""Dead-caller detector: public functions with no production caller.

This branch has paid for the same defect fourteen times (a function that is
correct, unit-tested, has a column and a screen branch -- and no caller). The
cheapest way to find the next one is to ask, across every Rust file on the
branch, which `fn` names are *defined* and never *called* outside their own
test module.

Two false-positive bugs were fixed in the first version and both are guarded
here, because a detector that cries wolf gets ignored:

  1. Counting the definition file's own references. A function is always
     "called" once by its own doc-link or its own `pub use`, so a file-local
     reference must be excluded rather than subtracted blindly (subtracting
     the whole file dropped 215 real callers on the first run).

  2. Counting mentions in doc comments and string literals. `/// See also
     capture()` is not a call site.

A third precision problem is that a `pub fn` in a `trait` block has no body and
is dispatched through the trait, so it is skipped entirely. An inherent method
in `impl Trait for Type` is dispatched through the trait too, so a match
anywhere in the workspace is accepted as its caller.

Usage:  python3 scripts/qa/dead-callers.py [--roots crates/workflows modules] [--json]
Exit code is always 0: this is a report, not a gate on its own. Each finding is
triaged by hand before anything is called a defect.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from collections import defaultdict
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]

# `fn` definitions we do not care about.
SKIP_DEF = re.compile(
    r"\bfn\s+(?P<name>[a-z_][a-z0-9_]*)\s*[<\[]"  # generic / where-bound
    r"|\bfn\s+(?P<name2>[a-z_][a-z0-9_]*)\s*\(",
)

CALL = re.compile(r"\b(?P<name>[a-z_][a-z0-9_]*)\s*(?:::|\.|\()")

# A path-qualified reference counts as a use even without parentheses:
# `ProjectRole::can_edit` is passed as a function VALUE (`permits(..., capability: fn(ProjectRole)
# -> bool)`), so the first version saw the whole capability matrix as dead —
# four of its five predicates, plus `can_administer`, which *is* used in
# production at `projects.rs:1118`.
QUALIFIED_USE = re.compile(r"\b[A-Z][A-Za-z0-9_]*::([a-z_][a-z0-9_]*)\b")

# A bare identifier passed AS A VALUE — `.map(scalar_to_string)`,
# `permits(pool, org, id, caller, ProjectRole::can_run)`. Neither of those has
# parentheses after the name, so the first version called `scalar_to_string`
# dead while `apply` maps over it on the module's only public entry point.
VALUE_USE = re.compile(r"(?:\(|,|=|\|)\s*(?:[A-Za-z_][\w]*::)*([a-z_][a-z0-9_]*)\s*[,)]")

# `#[test]`, `#[tokio::test]`, `#[tokio::test(flavor = "multi_thread")]` ...
TEST_ATTR = re.compile(r"^#\[[\w:]*test\b")

RUST_NOISE = (".rs",)


def strip_comments_and_strings(src: str) -> str:
    """Remove /* */ and // comments and string literals.

    The first version ran a plain `re.sub(r'/\\*.*?\\*/', re.S)`, which ate
    100 KB of a 102 KB file when a file's `/*` count exceeded its `*/` count
    (nested-looking doc examples). So the result is verified: if the
    substitution removed more than a third of the file it ran away, and the
    original is kept.
    """
    out = []
    i = 0
    n = len(src)
    while i < n:
        ch = src[i]
        two = src[i : i + 2]
        if two == "//":
            j = src.find("\n", i)
            i = n if j < 0 else j
            continue
        if two == "/*":
            depth = 1
            j = i + 2
            while j < n and depth:
                if src[j : j + 2] == "/*":
                    depth += 1
                    j += 2
                elif src[j : j + 2] == "*/":
                    depth -= 1
                    j += 2
                else:
                    j += 1
            out.append(" " * (j - i))
            i = j
            continue
        if ch in "\"'":
            quote = ch
            j = i + 1
            while j < n:
                if src[j] == "\\":
                    j += 2
                    continue
                if src[j] == quote:
                    j += 1
                    break
                j += 1
            out.append('""' if quote == '"' else "''")
            i = j
            continue
        out.append(ch)
        i += 1
    stripped = "".join(out)
    # Guard against the runaway substitution.
    if stripped and len(stripped) < (len(src) * 2) // 3:
        return src
    return stripped


def test_lines(lines: list[str], whole_file: bool = False) -> list[bool]:
    """Mark every line that lives in test code.

    A line is test code when it sits inside `#[cfg(test)] mod tests { .. }`,
    inside a `#[test]`-decorated `fn`, or anywhere in a file that lives under
    a `tests/` directory (integration tests have no marker at all — the first
    version reported every single one of their `#[tokio::test]` functions as a
    dead public API).

    The test attribute is matched as `#[<path>::test]` as well as `#[test]`:
    this workspace is async-first, so most tests are `#[tokio::test]` and the
    first version saw none of them.

    Computed ONCE per file by a single forward brace walk: an earlier version
    re-derived this per line with a backwards scan, which is O(lines^2) and
    made a whole-workspace scan take over three minutes.
    """
    if whole_file:
        return [True] * len(lines)
    out = [False] * len(lines)
    # One entry per OPEN brace, in order: True when the block it opened is a
    # test block. Every `{` pushes and every `}` pops, so a plain block nested
    # inside a test block pops itself rather than popping the test marker — the
    # first version pushed nothing for the nested block and every later test
    # in the file leaked into the production count.
    stack: list[bool] = []
    pending_attr = False
    for idx, line in enumerate(lines):
        stripped = line.strip()
        if TEST_ATTR.match(stripped):
            pending_attr = True
        elif not stripped or stripped.startswith("#"):
            pass  # keep a pending attribute through other attributes
        elif pending_attr:
            pending_attr = False

        is_test_fn = bool(pending_attr and re.search(r"\bfn\s+\w+", stripped))
        opens_test = (
            is_test_fn
            or bool(re.search(r"\bmod\s+(tests|test)\b", stripped))
            or stripped.startswith("#[cfg(test)]")
        )

        out[idx] = any(stack) or is_test_fn
        for _ in range(line.count("{")):
            stack.append(opens_test)
        for _ in range(line.count("}")):
            if stack:
                stack.pop()
    return out


def collect_files(roots: list[str]) -> list[Path]:
    files: list[Path] = []
    for root in roots:
        p = REPO / root
        if p.is_file() and p.suffix == ".rs":
            files.append(p)
        elif p.is_dir():
            files.extend(sorted(p.rglob("*.rs")))
    return files


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument(
        "--roots",
        nargs="+",
        default=["crates/workflows", "crates/ai-hub", "modules"],
        help="roots to scan for definitions (callers are matched workspace-wide)",
    )
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args()

    all_files = collect_files(["crates", "modules", "apps/api/src", "apps/admin"])
    root_files = collect_files(args.roots)

    # name -> list of (file, line) definitions
    defs: dict[str, list[tuple[Path, int, bool]]] = defaultdict(list)
    # name -> number of production call sites workspace-wide
    callers: dict[str, int] = defaultdict(int)
    # name -> number of test-only call sites (diagnostic only)
    test_callers: dict[str, int] = defaultdict(int)

    cache: dict[Path, str] = {}
    testmask: dict[Path, list[bool]] = {}
    for f in all_files:
        src = strip_comments_and_strings(f.read_text(encoding="utf-8", errors="replace"))
        cache[f] = src
        # Integration tests live under `tests/`, and this workspace also keeps
        # in-crate test modules as `…/tests.rs` (`mod tests;`) — both carry no
        # marker of their own.
        is_test_file = "tests" in f.parts or f.stem in ("tests", "test")
        testmask[f] = test_lines(src.split("\n"), whole_file=is_test_file)

    # ---- pass 1: definitions in the requested roots
    for f in root_files:
        src = cache[f]
        lines = src.split("\n")
        for idx, line in enumerate(lines):
            m = re.search(r"\b(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([a-z_][a-z0-9_]*)", line)
            if not m:
                continue
            name = m.group(1)
            # A trait method has no body: `fn name(...);`
            tail = line[m.end() :]
            if re.match(r"\s*(?:<[^;]*>)?\s*\([^;{]*\)\s*(?:where[^{;]*)?;", tail):
                continue
            # A declaration inside a trait block: previous non-empty line starts
            # with `trait ` / `pub trait `.
            k = idx - 1
            while k >= 0 and not lines[k].strip():
                k -= 1
            if k >= 0 and re.match(r"\s*(pub\s+)?(unsafe\s+)?trait\s+\w+", lines[k]):
                continue
            defs[name].append((f, idx + 1, testmask[f][idx]))

    # ---- pass 2: call sites workspace-wide
    for f, src in cache.items():
        lines = src.split("\n")
        for idx, line in enumerate(lines):
            names = {m.group("name") for m in CALL.finditer(line)}
            names |= {m.group(1) for m in QUALIFIED_USE.finditer(line)}
            names |= {m.group(1) for m in VALUE_USE.finditer(line)}
            for name in names:
                if name in ("fn", "let", "if", "match", "return", "use", "mod"):
                    continue
                # skip the definition line itself
                if re.search(r"\bfn\s+" + re.escape(name) + r"\b", line):
                    continue
                if testmask[f][idx]:
                    test_callers[name] += 1
                else:
                    callers[name] += 1

    findings = []
    for name, sites in sorted(defs.items()):
        prod = [s for s in sites if not s[2]]
        if not prod:
            continue
        if callers[name] > 0:
            continue
        findings.append(
            {
                "name": name,
                "definitions": [f"{s[0].relative_to(REPO)}:{s[1]}" for s in prod],
                "test_only_callers": test_callers[name],
                "callers": 0,
            }
        )

    if args.json:
        print(json.dumps(findings, indent=2))
        return 0

    print(f"dead-caller scan: {len(defs)} distinct fn names defined under {args.roots}")
    print(f"workspace files read: {len(all_files)}")
    if not findings:
        print("no public function without a production caller")
        return 0
    print(f"\n{len(findings)} candidate(s) -- each needs hand triage:\n")
    for f in findings:
        where = ", ".join(f["definitions"][:4])
        print(f"  {f['name']:<34} {where}")
        if f["test_only_callers"]:
            print(f"  {'':<34} (called {f['test_only_callers']}x in test code only)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
