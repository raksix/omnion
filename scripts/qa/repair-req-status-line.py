#!/usr/bin/env python3
"""Restore the structural anchors a REQ status-line write can eat.

**What this is for.** The status line of a request file is rewritten by hand (or by a
script that edits the first `> **Status:**` paragraph). When the writer substitutes the
*whole* leading block instead of the status line alone, the file loses the parts that
made it a request at all: the `# REQ-NNN - Title` heading, the `## Request` heading, and
the summary sentence. What is left is a status line that talks about some other request.

**The shape of the damage.** A request file starts

    # REQ-117 - Forms -> CRM Lead Pipeline

    > **Status:** in-progress ... · **Captured:** 2026-09-26 · **Layer:** modules/...

    ## Request

    The website/business integration that sells the platform.

After the bad write it starts

    > **Status:** in-progress - **SLICE 44 (2026-10-01): tick 75 closed on ...

which is the *previous* request's prose, pasted where the title was. The rest of the file
(`## Implementation spec`, the slices, the acceptance list) is untouched, which is why the
file still typechecks as markdown and still looks plausible to a reader skimming.

**Why the anchors are rebuilt rather than restored from git.** `git show <commit>:<path>`
gives the file as it was at a commit, and the obvious repair is to take the first N lines
from an older commit. That copies whatever the status line said *then* and throws away
every slice recorded since, which is the part a reader actually needs. So the title, the
`## Request` heading and the summary are recovered from the newest ancestor that still has
them, and the *current* status text is re-attached underneath the title, so nothing is
lost and the corruption is visible as a duplicate status paragraph rather than hidden.

**Usage**

    python3 scripts/qa/repair-req-status-line.py --check
    python3 scripts/qa/repair-req-status-line.py --apply

`--check` exits non-zero and names the files when any is damaged, so it can run in CI.
`--apply` rewrites them and prints what it did, one line per file.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

REQS_DIR = Path("docs/requests")
TITLE_RE = re.compile(r"^# (REQ-\d+) — (.+)$")
STATUS_RE = re.compile(r"^> \*\*Status:\*\*")
REQUEST_RE = re.compile(r"^## Request\s*$")

# A status paragraph is the `> ` block that follows the title, and it may be long: the
# convention on this branch is to record the slice's finding in full, so several hundred
# lines of prose live inside one blockquote.
STATUS_BLOCK = 4_000


def git_show(rev: str, path: str) -> str | None:
    proc = subprocess.run(
        ["git", "show", f"{rev}:{path}"],
        capture_output=True,
        text=True,
        check=False,
    )
    return proc.stdout if proc.returncode == 0 else None


def newest_ancestor_with_anchors(path: str, depth: int = 400) -> str | None:
    """The most recent commit whose copy of `path` still has title + `## Request`."""
    revs = subprocess.run(
        ["git", "log", f"--max-count={depth}", "--format=%H", "--", path],
        capture_output=True,
        text=True,
        check=False,
    ).stdout.split()
    for rev in revs:
        content = git_show(rev, path)
        if not content:
            continue
        lines = content.splitlines()
        if not any(TITLE_RE.match(line) for line in lines):
            continue
        if not any(REQUEST_RE.match(line) for line in lines):
            continue
        return rev
    return None


def status_text(content: str) -> str:
    """The status prose currently in the file, minus the `> ` markers."""
    out: list[str] = []
    seen = False
    for line in content.splitlines():
        if STATUS_RE.match(line):
            seen = True
            out.append(line)
            continue
        if not seen:
            continue
        if line.startswith(">"):
            out.append(line)
            continue
        if not line.strip():
            # A blank line ends the blockquote, but only once prose has started: a status
            # line followed by a blank line and then a heading is a one-line status.
            if out:
                break
            continue
        # A non-quote, non-blank line: the status block ended above it.
        break
    return "\n".join(out).strip()


def repair(content: str, anchors: str) -> tuple[str, bool]:
    """Return (repaired content, changed?)."""
    lines = content.splitlines()

    has_title = any(TITLE_RE.match(line) for line in lines)
    has_request = any(REQUEST_RE.match(line) for line in lines)
    if has_title and has_request:
        return content, False

    # Recover the two anchors plus the summary that sat between them.
    anchor_lines = anchors.splitlines()
    title = next(line for line in anchor_lines if TITLE_RE.match(line))
    request_at = next(
        i for i, line in enumerate(anchor_lines) if REQUEST_RE.match(line)
    )
    summary = ""
    for line in anchor_lines[request_at + 1 :]:
        if line.strip():
            summary = line.strip()
            break

    status = status_text(content)

    header = [title, ""]
    if status:
        header.append(status)
        header.append("")
    header.append("## Request")
    header.append("")
    if summary:
        header.append(summary)

    # Drop the damaged leading block: everything before the first `## Implementation
    # spec` heading is header, and the whole of it is the part that was corrupted.
    spec_at = next(
        (
            i
            for i, line in enumerate(lines)
            if line.startswith("## Implementation spec")
        ),
        None,
    )
    body = lines[spec_at:] if spec_at is not None else lines

    return "\n".join(header + body).rstrip() + "\n", True


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true")
    ap.add_argument("--apply", action="store_true")
    args = ap.parse_args()

    if not (args.check or args.apply):
        ap.error("pass --check or --apply")

    damaged: list[tuple[Path, str, str]] = []
    for path in sorted(REQS_DIR.glob("REQ-*.md")):
        content = path.read_text(encoding="utf-8", errors="replace")
        lines = content.splitlines()
        intact = any(TITLE_RE.match(line) for line in lines) and any(
            REQUEST_RE.match(line) for line in lines
        )
        if intact:
            continue
        rel = str(path)
        anchor_rev = newest_ancestor_with_anchors(rel)
        if anchor_rev is None:
            damaged.append((path, "", "no ancestor commit retains the anchors"))
            continue
        repaired, changed = repair(content, git_show(anchor_rev, rel) or "")
        if not changed:
            continue
        if args.apply:
            path.write_text(repaired, encoding="utf-8")
        damaged.append((path, anchor_rev, "repaired" if args.apply else "would repair"))

    if not damaged:
        print("req-status-line: all request files carry their title and '## Request'")
        return 0

    for path, rev, what in damaged:
        print(f"req-status-line: {path.name}: {what} (anchors from {rev[:8] or 'n/a'})")

    if args.apply:
        print(f"req-status-line: {len(damaged)} file(s) rewritten")
        return 0
    return 1


if __name__ == "__main__":
    sys.exit(main())
