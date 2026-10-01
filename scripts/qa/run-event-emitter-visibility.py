#!/usr/bin/env python3
"""The event registry must agree with the emitters, in BOTH directions — and a name that
reaches the bus through a wrapper is invisible to the walk that checks it.

    python3 scripts/qa/run-event-emitter-visibility.py

## Why this gate exists

`apps/api/tests/events.rs` walks the workspace looking for a quoted string at a
`NewEvent::new(` / `Announcement::new(` constructor, and it is a good gate: it found two
`Live` rows with nothing behind them, and it names them. It is also a **source scanner**,
and this tick found the shape it cannot see.

A name that reaches `bus::emit` as a *variable* is not a literal at any constructor, so the
walk never collects it and the catalogue row reads as `Live` with no emitter. Two such names
existed on this branch at once:

* `automation.project.limit.warning` / `.limit_exceeded` — built by a `match` three lines
  above the constructor and handed over as `NewEvent::new(name)`. The rows were correct, the
  worker emitted both names correctly at runtime, and the gate was **red from the day they
  shipped** because the tick that wrote them ran a different suite.
* `crm.intake.rule.updated` — passed as a parameter to a local `emit(…)` helper that calls
  `bus::emit` internally. This one is worse than red: the name had **no catalogue row at
  all**, so the picker never offered it and the event was undeliverable by construction while
  the bus recorded it on every assignment-rule write.

Both are the same defect, and neither is visible to a walk that reads constructors.

## What this gate checks, and what it deliberately does not

It does **not** reimplement the walk, and it does not try to resolve bindings — a scanner
that follows a variable would follow a `format!` too, and then it can no longer name a wire
contract assembled at runtime. The honest checks are:

1. **Every bus-emitted name literal is in the catalogue.** Read at the literal's own call
   site, including a local wrapper's `emit(pool, org, "name", …)` shape.
2. **No wrapper hides a name parameter.** A helper whose body reaches `bus::emit` and whose
   signature takes a `name`-shaped parameter is flagged, because that is the shape that made
   both of the names above invisible. The repair is to delete the parameter and write the
   literal at the constructor; the helper grows a second constructor rather than a parameter.
3. **The suite's own gate is not red.** Runs `cargo test -p omnion-api --test events every_`,
   so a red drift gate cannot sit on the branch for a tick.

Check 2 is a *shape* check rather than a name check, which is the only way to see a wrapper
that has not been written yet. Its positive control is the shape's own past: both fixed
names now appear as literals, and re-introducing the parameter makes this gate red again.

## Reading the output

Each line is `PASS`/`FAIL` plus what it looked at. A failure names the file and line so it
can be opened directly; there is no summary count that could hide a failure inside a tally.

Exit code 0 when every check passes, 1 otherwise.
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
AREAS = ("apps", "crates", "modules")
SKIP_DIRS = {"target", "node_modules", ".git"}

# Constructors the walk in apps/api/tests/events.rs recognises as emitters.
CONSTRUCTORS = ("NewEvent::new(", "Announcement::new(")

# `bus::emit` reached through a *local* helper: `emit(pool, organization_id, "name", …)`.
WRAPPER_CALL = re.compile(
    r"(?<![\w:.])emit\(\s*pool\s*,\s*\w+\s*,\s*\"([a-z][a-z0-9_]*(?:\.[a-z0-9_]+)+)\""
)

# A helper that reaches the bus and takes the name as a parameter — the blind spot.
HELPER_FN = re.compile(r"(?:pub\s+)?(?:async\s+)?fn\s+(\w+)\s*\(", re.M)
NAME_PARAM = re.compile(r"(?:name|event_name|event)\s*:\s*&'?static str")


def code_of(line: str) -> str:
    """A line with its trailing `//` comment removed (same rule as the Rust walk)."""
    return line.split("//", 1)[0]


def in_test_module(lines: list[str], index: int) -> bool:
    """Whether line `index` sits inside a `#[cfg(test)] mod` — fixtures are not emitters."""
    flag = False
    for line in lines[:index]:
        code = code_of(line)
        if code.lstrip().startswith("#[cfg(test)]") or "#[cfg(all(test" in code:
            flag = True
        if flag and (code.startswith("}") or code.startswith("//!")):
            flag = False
    return flag


def catalogue_names() -> dict[str, str]:
    """`name -> status` for every catalogue row."""
    text = (ROOT / "crates/events/src/catalogue.rs").read_text()
    found = {}
    for match in re.finditer(
        r'"([a-z][a-z0-9_]*(?:\.[a-z0-9_]+)+)",\s*"[a-z]+",\s*(Live|Reserved)', text
    ):
        found[match.group(1)] = match.group(2)
    if not found:
        print("FAIL  the catalogue parsed to zero rows; the regex is wrong, not the table")
    return found


def walk_files() -> list[Path]:
    files: list[Path] = []
    for area in AREAS:
        base = ROOT / area
        if not base.is_dir():
            continue
        for path in base.rglob("*.rs"):
            if any(part in SKIP_DIRS for part in path.parts):
                continue
            files.append(path)
    return files


def emitted_literals(files: list[Path]) -> list[tuple[str, str]]:
    """Every event name read at a constructor or at a local wrapper's call site."""
    found: list[tuple[str, str]] = []
    for path in files:
        text = path.read_text(errors="ignore")
        lines = text.split("\n")
        for index, line in enumerate(lines):
            code = code_of(line)
            if in_test_module(lines, index):
                continue
            where = f"{path.relative_to(ROOT)}:{index + 1}"

            for constructor in CONSTRUCTORS:
                if constructor not in code:
                    continue
                after = code.split(constructor, 1)[1]
                # The name is the text up to the *next* quote. Splitting the whole tail on a
                # quote keeps the closing one glued to the name (`crm.lead.received")`), so the
                # terminator has to be stripped rather than assumed absent — `NewEvent::new("a")`
                # and `NewEvent::new("a", json!(…))` are the same emission.
                quoted = after.split('"', 2)
                if len(quoted) < 2:
                    continue
                candidate = quoted[1]
                if candidate.count(".") >= 1 and re.fullmatch(r"[a-z][a-z0-9_.]*", candidate):
                    found.append((candidate, where))

            for name in WRAPPER_CALL.findall(code):
                found.append((name, where))

            # A wrapper that calls a helper taking the name: `emit(pool, org, name, …)`.
            if re.search(r"(?<![\w:.])emit\(\s*pool\s*,\s*\w+\s*,\s*name\b", code):
                found.append(("<via name binding>", where))

    return found


def wrappers_with_a_name_parameter(files: list[Path]) -> list[str]:
    """Helpers that reach `bus::emit` while taking the name as a parameter."""
    offenders = []
    for path in files:
        text = path.read_text(errors="ignore")
        for match in HELPER_FN.finditer(text):
            name = match.group(1)
            header_end = text.find(")", match.end())
            if header_end == -1:
                continue
            header = text[match.start() : header_end + 1]
            if not NAME_PARAM.search(header):
                continue
            body = text[header_end : header_end + 1200]
            if "bus::emit" not in body:
                continue
            line = text[: match.start()].count("\n") + 1
            offenders.append(f"{path.relative_to(ROOT)}:{line} fn {name}")
    return offenders


def run_the_rust_gate() -> tuple[bool, str]:
    """Run the suite's own drift gate so a red one cannot sit on the branch."""
    result = subprocess.run(
        ["cargo", "test", "-p", "omnion-api", "--test", "events", "every_", "--quiet"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        timeout=1800,
    )
    tail = [line for line in result.stdout.splitlines() if "test result" in line]
    return result.returncode == 0, (tail[-1].strip() if tail else "no test result line")


def main() -> int:
    failures = 0
    files = walk_files()
    catalogue = catalogue_names()
    print(f"      walked {len(files)} Rust files, {len(catalogue)} catalogue rows")

    emitted = emitted_literals(files)
    if not emitted:
        print("FAIL  the walk found no emitters at all; it is broken, not the branch")
        return 1

    unlisted = sorted(
        {(name, where) for name, where in emitted if name not in catalogue and "<" not in name}
    )
    for name, where in unlisted:
        print(f"FAIL  emitted but not in the catalogue: {name}  ({where})")
        failures += 1

    bindings = sorted({where for name, where in emitted if "<" in name})
    for where in bindings:
        print(
            f"FAIL  an emitter hands the bus a name it never wrote at a constructor: {where}\n"
            f"        the walk cannot see it, so its catalogue row reads as Live with no\n"
            f"        emitter — delete the `name` parameter and write the literal instead"
        )
        failures += 1

    offenders = wrappers_with_a_name_parameter(files)
    for offender in offenders:
        print(
            f"FAIL  a wrapper takes the event name as a parameter, hiding it from the walk: "
            f"{offender}"
        )
        failures += 1

    # The reverse direction is the Rust gate's job; running it here is what keeps a red gate
    # from sitting on the branch for a tick unnoticed.
    ok, summary = run_the_rust_gate()
    print(f"{'PASS' if ok else 'FAIL'}  the suite's own drift gate: {summary}")
    if not ok:
        failures += 1

    if failures:
        print(f"      {failures} failure(s)")
        return 1
    print("      every emitted name is catalogueued and visible to the walk")
    return 0


if __name__ == "__main__":
    sys.exit(main())
