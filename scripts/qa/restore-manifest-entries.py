#!/usr/bin/env python3
"""Restore the manifest and module declarations the fleet merge overwrote.

WHY
Each writer loop owned its waves, so a loop's `Cargo.toml`, `lib.rs` and `mod.rs` listed
only its own crates and modules. A file-based union keeps one version of each — the
newest branch that had it — so the shared registration files lost most of their entries.
Nothing referenced those modules any more, and the build failed with a wall of
"cannot find module or crate" errors that each pointed at a leaf file rather than at the
manifest that had dropped it.

WHAT IT DOES, IN ORDER
1. Every path dependency under `crates/` and `modules/` that is missing from a
   manifest is added back, with the crate name read from the dependency's own
   `Cargo.toml` (`[package] name`) — not guessed from the directory name.
2. `[[bin]]` and `[lib] name` are not touched: those are declared explicitly and a
   wrong guess there is worse than a missing entry.
3. Every `src/*.rs` file that is not declared as a module in its `lib.rs` (or
   `main.rs`) and lives beside a module that IS declared is added, so the module
   files the build complains about are reachable.

Run with --check to report only.
"""
from __future__ import annotations

import os
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SRC = ROOT / "apps" / "api" / "src"

PKG_NAME = re.compile(r"^\s*name\s*=\s*[\"']([^\"']+)[\"']", re.M)
SECTION = re.compile(r"^\[([^\]]+)\]", re.M)


def package_name(manifest: Path) -> str | None:
    """Read [package] name out of a crate manifest, not the directory name."""
    try:
        text = manifest.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None
    m = SECTION.search(text)
    if not m or m.group(1).strip() != "package":
        return None
    tail = text[m.end():]
    nxt = SECTION.search(tail)
    body = tail[: nxt.start()] if nxt else tail
    n = PKG_NAME.search(body)
    return n.group(1) if n else None


LOCAL_CRATES = (ROOT / "crates", ROOT / "modules")


def _local_names() -> dict[str, Path]:
    """Every workspace crate's package name and directory, resolved once."""
    out: dict[str, Path] = {}
    for base in LOCAL_CRATES:
        if not base.is_dir():
            continue
        for d in sorted(base.iterdir()):
            m = d / "Cargo.toml"
            if d.is_dir() and m.is_file():
                n = package_name(m)
                if n:
                    out[n] = d
    return out


ALL_LOCAL = _local_names()
ALL_LOCAL_NAMES = sorted(ALL_LOCAL)


def manifests() -> list[Path]:
    """Every Cargo.toml in the workspace: the API and every crate under crates/ or modules/.

    The merge overwrote each writer's own lib.rs and Cargo.toml, so the damage is not
    confined to apps/api — every crate that another branch also touched lost its module
    list or its sibling dependencies. Restoring only the API is what produced a wall of
    "unresolved import omnion_ai_hub::agent": the import resolves the crate but the
    crate's own lib.rs no longer declares the module.
    """
    out = [ROOT / "apps" / "api" / "Cargo.toml"]
    for base in LOCAL_CRATES:
        if base.is_dir():
            for d in sorted(base.iterdir()):
                m = d / "Cargo.toml"
                if d.is_dir() and m.is_file():
                    out.append(m)
    return out


def used_crates(manifest: Path) -> set[str]:
    """The workspace crates this manifest's source actually references.

    A blanket "add every crate" pass is wrong: a crate that declares a dependency it
    never imports is not harmless, it is a cycle waiting to happen — and here the
    module crates all import each other, so the blanket pass produced a web of edges
    cargo cannot resolve. The dependency has to come from the code, not from the
    existence of a sibling directory: the crate name is a prefix of every path the
    source refers to (`omnion_module_crm::Pipeline`, `use omnion_workflows::...`).
    """
    root = manifest.parent
    names: set[str] = set()
    for src in root.rglob("*.rs"):
        if "target" in src.parts or ".next" in src.parts:
            continue
        try:
            text = src.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        for n in ALL_LOCAL_NAMES:
            if re.search(rf"\b{re.escape(n)}\s*::", text) or re.search(
                    rf"\b{re.escape(n)}\b", text):
                names.add(n)
    return names


def restore_dependencies(manifest: Path, report: list[str]) -> bool:
    text = manifest.read_text(encoding="utf-8")

    own = package_name(manifest)
    deps = {}
    for base in LOCAL_CRATES:
        if not base.is_dir():
            continue
        for d in sorted(base.iterdir()):
            m = d / "Cargo.toml"
            if d.is_dir() and m.is_file():
                name = package_name(m)
                if not name or d == manifest.parent:
                    continue
                rel = os.path.relpath(d, manifest.parent).replace(os.sep, "/")
                deps[name] = rel

    wanted = used_crates(manifest) & set(deps)
    if own:
        wanted.discard(own)

    lines = text.splitlines()
    section_at = []
    cur = ""
    for ln in lines:
        m = re.match(r"^\[([^\]]+)\]", ln)
        if m:
            cur = m.group(1).strip()
        section_at.append(cur)

    additions = [(n, deps[n]) for n in sorted(wanted)
                 if not re.search(rf"^\s*{re.escape(n)}\s*=", text, re.M)]

    if not additions:
        return False

    last = max((i for i, s in enumerate(section_at) if s.endswith("dependencies")),
               default=None)
    if last is None:
        return False
    for name, rel in additions:
        lines.insert(last + 1, f'{name} = {{ path = "{rel}" }}')
        report.append(f"{manifest.relative_to(ROOT)}: +{name}")
    manifest.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return True


def declared_modules(rs: Path) -> set[str]:
    if not rs.is_file():
        return set()
    text = rs.read_text(encoding="utf-8", errors="replace")
    return set(re.findall(r"^\s*(?:pub\s+)?mod\s+([a-zA-Z0-9_]+)\s*;", text, re.M))


def restore_modules(report: list[str]) -> bool:
    """A file under src/ is a module only if its mod.rs says so. Add the missing ones.

    Every directory that holds Rust modules has exactly one registration file — lib.rs
    for the crate root, mod.rs for a subdirectory. The fleet merge overwrote several of
    them, so the fix has to sweep every registration file, not just the crate root: a
    module that is present and undeclared fails the build exactly like a missing one.
    """
    changed = False
    for dirpath, dirnames, filenames in os.walk(SRC):
        dirnames[:] = [d for d in dirnames if d not in ("target", ".next", "node_modules")]
        d = Path(dirpath)
        reg = d / "mod.rs" if (d / "mod.rs").is_file() else None
        if reg is None and d.name != "src":
            continue
        if reg is None:
            continue
        text = reg.read_text(encoding="utf-8", errors="replace")
        have = set(re.findall(r"^\s*(?:pub\s+)?mod\s+([a-zA-Z0-9_]+)\s*;", text, re.M))
        added = []
        for f in sorted(d.glob("*.rs")):
            if f.name in ("mod.rs", "lib.rs", "main.rs"):
                continue
            stem = f.stem
            if stem in have or re.search(rf"^\s*(?:pub\s+)?mod\s+{re.escape(stem)}\s*;", text, re.M):
                continue
            added.append(stem)
        if not added:
            continue
        text = text.rstrip("\n") + "\n" + "".join(
            f"\npub mod {s};" for s in added
        ) + "\n"
        reg.write_text(text, encoding="utf-8")
        for s in added:
            report.append(f"{reg.relative_to(ROOT)}: +mod {s}")
        changed = True
    return changed


def main() -> int:
    check = "--check" in sys.argv
    report: list[str] = []

    if check:
        missing_total = 0
        for man in manifests():
            text = man.read_text(encoding="utf-8")
            names = []
            for base in LOCAL_CRATES:
                if not base.is_dir():
                    continue
                for d in sorted(base.iterdir()):
                    m = d / "Cargo.toml"
                    if d.is_dir() and m.is_file() and d != man.parent:
                        n = package_name(m)
                        if n:
                            names.append(n)
            gone = [n for n in names
                    if not re.search(rf"^\s*{re.escape(n)}\s*=", text, re.M)]
            if gone:
                missing_total += len(gone)
                print(f"  {man.relative_to(ROOT)}: {len(gone)} missing")
                for n in sorted(gone):
                    print(f"    {n}")
        if not missing_total:
            print("  every local crate is declared")
        return 1 if missing_total else 0

    for man in manifests():
        restore_dependencies(man, report)
    restore_modules(report)
    for r in report:
        print(f"  {r}")
    print(f"  {len(report)} restorations")
    return 0


if __name__ == "__main__":
    sys.exit(main())
