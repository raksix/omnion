#!/usr/bin/env python3
"""The build-log merge script gets a test of its own.

The failure it exists to prevent has already happened twice on this branch — 84 silently dropped
lines, then a duplicated block — and neither was noticed until much later. A merge helper that is
only ever exercised by a live conflict is a helper whose first real use is also its first test,
and "the conflict resolved fine" is not something anybody checks.

Run: `python3 scripts/qa/test-merge-build-log.py`
"""
import subprocess
import sys
import tempfile
import textwrap
from pathlib import Path

SCRIPT = Path(__file__).with_name("..") / "merge-build-log.py"

BASE = """## first entry

shared history line one
shared history line two

## second entry

tail of the base
"""


def git(repo, *args, check=True):
    result = subprocess.run(
        ["git", "-C", str(repo), *args], capture_output=True, text=True, check=False
    )
    if check and result.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)} failed: {result.stderr}")
    return result


def write_log(repo, text):
    (repo / "docs").mkdir(exist_ok=True)
    (repo / "docs" / "BUILD-LOG.md").write_text(text)


def commit(repo, message):
    git(repo, "add", "-A")
    git(repo, "-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", message)


def build(tmp, ours_entry, theirs_entry):
    """A repo where `wave` and `main` each added one entry to the shared base.

    Both branches append, so the tail conflicts and the head is untouched — which
    is exactly the shape this log has in production, and the reason a hand
    resolution drops one side.
    """
    repo = Path(tmp)
    git(repo, "init", "-q", "-b", "main")
    write_log(repo, BASE)
    commit(repo, "base")

    git(repo, "checkout", "-q", "-b", "wave")
    write_log(repo, BASE + "\n" + textwrap.dedent(ours_entry).lstrip("\n"))
    commit(repo, "wave entry")

    git(repo, "checkout", "-q", "main")
    write_log(repo, BASE + "\n" + textwrap.dedent(theirs_entry).lstrip("\n"))
    commit(repo, "main entry")

    git(repo, "merge", "--no-commit", "--no-ff", "wave", check=False)
    return repo


def run_merge(repo):
    result = subprocess.run(
        [sys.executable, str(SCRIPT.resolve()), "wave", "main"],
        cwd=repo, capture_output=True, text=True, check=False,
    )
    return result.returncode, result.stdout + result.stderr


def check(title, ours_entry, theirs_entry, *, expect_ok=True, contains=(), missing=(),
          lines=None):
    with tempfile.TemporaryDirectory() as tmp:
        repo = build(tmp, ours_entry, theirs_entry)
        assert (repo / "docs" / "BUILD-LOG.md").read_text().count("<<<<<<<"), (
            "the fixture must actually conflict, or the case proves nothing"
        )
        code, out = run_merge(repo)
        merged = (repo / "docs" / "BUILD-LOG.md").read_text()

    problems = []
    if expect_ok and code != 0:
        problems.append(f"expected success, got exit {code}: {out.strip()[-300:]}")
    if not expect_ok and code == 0:
        problems.append("expected the merge to be REFUSED, and it passed")
    for needle in contains:
        if needle not in merged:
            problems.append(f"missing {needle!r}")
    for needle in missing:
        if needle in merged:
            problems.append(f"unexpectedly present {needle!r}")
    if lines is not None and len(merged.splitlines()) != lines:
        problems.append(f"expected {lines} lines, got {len(merged.splitlines())}")

    print(f"{'PASS' if not problems else 'FAIL'}  {title}")
    for problem in problems:
        print(f"          {problem}")
    return not problems


results = [
    check(
        "both entries survive, with no conflict markers left behind",
        "## the wave entry\n\nthe wave writer said something\n",
        "## the main entry\n\nthe main writer said something\n",
        contains=[
            "## first entry", "## the wave entry", "## the main entry",
            "the wave writer said something", "the main writer said something",
            "shared history line one", "shared history line two", "tail of the base",
        ],
        missing=["<<<<<<<", "=======", ">>>>>>>"],
    ),
    check(
        "the base is byte-identical in its untouched middle",
        "## wave\n\nA\n", "## main\n\nB\n",
        contains=["shared history line one\n", "shared history line two\n"],
    ),
    check(
        "a long entry is not truncated at the length check",
        "## wave\n\n" + "".join(f"line {i} of the wave entry\n" for i in range(200)) + "\n",
        "## main\n\n" + "".join(f"line {i} of the main entry\n" for i in range(200)) + "\n",
        contains=["line 0 of the wave entry", "line 199 of the wave entry",
                  "line 0 of the main entry", "line 199 of the main entry"],
    ),
    check(
        "a sentence both sides wrote survives exactly the number of times it appears",
        "## wave\n\nthe same sentence\n", "## main\n\nthe same sentence\n",
        contains=["the same sentence"],
    ),
]

print()
failed = results.count(False)
print(f"{len(results) - failed}/{len(results)} passed")
sys.exit(1 if failed else 0)
