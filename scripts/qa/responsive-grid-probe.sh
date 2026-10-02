#!/usr/bin/env bash
# Prove that no wave-4 business screen keeps a fixed multi-column grid at phone width.
#
# Why this is a probe and not a review pass
# -----------------------------------------
# A Tailwind `sm:`/`lg:` prefix is what makes a grid collapse to one column on a phone, and reading
# the class list is exactly as convincing as it looks: `grid-cols-2` inside `sm:grid-cols-2` is a
# one-column layout at 390px. Three of the four grids this found carried an `sm:` prefix somewhere in
# the string and were therefore correct. What separates a defect from a correct layout is a single
# character — whether the multi-column token is preceded by a breakpoint — and the only reliable way
# to tell is to tokenise the class string once and ask "is this token always on?".
#
# So the rule is checked structurally: for every `className="…"` string in the wave-4 modules, every
# `grid-cols-N` / `grid-flow-col` / `flex-row` token must carry a breakpoint prefix or be
# `grid-cols-1`. The lookbehind excludes `:` (the prefix separator), `[\w-]` (so `sm:grid-cols-2`
# cannot match as `grid-cols-2`) and the token must be followed by a word boundary.
#
# The scan is over the **source**, not the rendered box, so it cannot be passed by a media query that
# never fires or by a screen that happens to look right at the one width nobody checks.
#
# Usage: bash scripts/qa/responsive-grid-probe.sh [dir ...]   (default: the four wave-4 modules)

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DIRS=("$@")
if [ ${#DIRS[@]} -eq 0 ]; then
  DIRS=(crm sales inventory accounting)
fi

python3 - "$ROOT" "${DIRS[@]}" <<'PY'
import os, re, sys

root, dirs = sys.argv[1], sys.argv[2:]
base = os.environ.get("RG_BASE") or os.path.join(root, "apps/admin/features")

CLASSNAME = re.compile(r'className="([^"]*)"')
# `(?<![\w:-])` — a token preceded by a letter/digit/underscore, a hyphen or a colon is NOT always-on:
# `sm:grid-cols-2` and `grid-cols-12` both have to be rejected, and a plain `grid-cols-2` kept.
ALWAYS_ON = re.compile(r'(?<![\w:-])(grid-cols-(\d)|grid-flow-col|flex-row)(?![\w-])')

findings = []
scanned = 0
for mod in dirs:
    folder = os.path.join(base, mod)
    if not os.path.isdir(folder):
        print(f"!! no such module: {mod}")
        continue
    for name in sorted(os.listdir(folder)):
        if not name.endswith(".tsx"):
            continue
        path = os.path.join(folder, name)
        src = open(path, encoding="utf-8").read()
        scanned += 1
        for m in CLASSNAME.finditer(src):
            cls = m.group(1)
            for tok in ALWAYS_ON.finditer(cls):
                if tok.group(1) == "grid-cols-1":
                    continue
                line = src[: m.start()].count("\n") + 1
                findings.append((mod, name, line, tok.group(1), cls.strip()))

if not findings:
    print(f"responsive-grid-probe: {scanned} files scanned, 0 always-on multi-column grids")
    raise SystemExit(0)

for mod, name, line, tok, cls in findings:
    print(f"FAIL {mod}/{name}:{line}  {tok} has no breakpoint prefix")
    print(f"       className=\"{cls}\"")
print(f"responsive-grid-probe: {len(findings)} defect(s) in {scanned} files")
raise SystemExit(1)
PY
