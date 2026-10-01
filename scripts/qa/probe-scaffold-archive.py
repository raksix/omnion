#!/usr/bin/env python3
"""
Open a scaffold archive with a reader that is not ours (REQ-033, slice 4).

`crates/developer/src/archive.rs` ships its own reader so its unit tests are not asking the
writer to check its own homework. That is necessary and it is not sufficient: a writer and a
reader written by the same hand agree on their own mistake. This probe takes the *bytes* a
cargo test produces and opens them with Python's `zipfile`, which is an independent
implementation, and then with `/usr/bin/unzip`, which is another.

Why three readers for one format is not paranoia: the failure mode is silent. A zip with a
correct local header, a correct payload and a wrong relative offset in the central directory
still *reads* — it just cannot be extracted, and a developer finds out after downloading, on
their own machine, having already left the platform. The `unzip -t` run is the one that would
catch a corrupt entry; the `zipfile` run is the one that would catch a header a strict reader
rejects.

Usage:  python3 scripts/qa/probe-scaffold-archive.py [--keep DIR]
Exit 0 when every check held, 1 otherwise, with the failing check named.
"""

from __future__ import annotations

import argparse
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]

# The example that writes the bytes. It lives in the developer crate so the archive is produced
# by the same `generate` + `zip` the API calls, rather than by a fixture in this file — a
# hand-written archive here would prove only that Python can read a Python literal.
EXAMPLE = "archive_probe"
# `CARGO_TARGET_DIR` is not a constant on this box: this branch builds into `/dev/shm/w5target`
# because the worktree volume sits at 92% with seven writers on it. Hardcoding `target/` here
# would make the probe look for a binary that was never built there and report a missing file
# rather than a missing build — the failure would point at the filesystem instead of at cargo.
TARGET_DIR = Path(os.environ.get("CARGO_TARGET_DIR") or (REPO / "target"))

failures: list[str] = []
checks = 0


def check(name: str, ok: bool, detail: str = "") -> None:
    global checks
    checks += 1
    if ok:
        print(f"  ok   {name}")
    else:
        print(f"  FAIL {name}" + (f" — {detail}" if detail else ""))
        failures.append(name)


def build_archive() -> bytes:
    """Compile and run the dumper, return the archive's bytes.

    The dumper is an `example` in the developer crate so it can reach the crate's own
    `generate` + `zip` with no duplicated template text. If it is missing this probe fails
    loudly rather than testing a hand-written archive that proves nothing.

    It is built **every time** rather than behind an mtime check: the example links the crate,
    so a `stale example, fresh library` comparison is exactly the case where the binary on disk
    predates the writer it is supposed to be proving, and the probe would open last week's zip
    and report it green.
    """
    src = REPO / "crates/developer/examples" / f"{EXAMPLE}.rs"
    if not src.exists():
        raise SystemExit(f"missing {src}")

    env = dict(os.environ)
    env.setdefault("CARGO_TARGET_DIR", str(TARGET_DIR))
    env["PATH"] = f"{Path.home() / '.cargo/bin'}:{env.get('PATH', '')}"
    build = subprocess.run(
        ["cargo", "build", "-p", "omnion-developer", "--example", EXAMPLE],
        cwd=REPO,
        env=env,
        capture_output=True,
        text=True,
    )
    if build.returncode != 0:
        print(build.stdout[-4000:])
        print(build.stderr[-4000:], file=sys.stderr)
        raise SystemExit("the archive dumper did not build")

    # Cargo puts an `example` under `<target>/<profile>/examples/`, NOT next to the deps — an
    # example is a target like a binary, and the directory is part of the contract. Looking in
    # `<profile>/` finds the *dependencies* but never the example, which reads as "cargo built
    # no archive_probe" and sends whoever is reading it looking for a build failure that did not
    # happen. The profile is read rather than assumed so a `--release` run finds the release
    # binary instead of failing on a missing debug one.
    profile = "release" if "--release" in sys.argv else "debug"
    binary = TARGET_DIR / profile / "examples" / EXAMPLE
    if not binary.exists():
        raise SystemExit(f"cargo built no {EXAMPLE} at {binary}")

    run = subprocess.run(
        [str(binary)], cwd=REPO, capture_output=True, text=True, env=env
    )
    if run.returncode != 0:
        print(run.stdout, run.stderr, file=sys.stderr)
        raise SystemExit("the archive dumper did not run")
    return Path(run.stdout.strip()).read_bytes()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--keep",
        help="leave the archive and its extraction on disk at this path (for a human to open)",
    )
    args = parser.parse_args()

    raw = build_archive()
    out = Path(args.keep) if args.keep else Path(tempfile.mkdtemp(prefix="scaffold-archive-"))
    out.mkdir(parents=True, exist_ok=True)
    archive_path = out / "omnion-plugin-plugin-probe-live.zip"
    archive_path.write_bytes(raw)
    print(f"archive: {len(raw)} bytes at {archive_path}")

    print("\n[zipfile] an independent reader")
    try:
        with zipfile.ZipFile(archive_path) as zf:
            bad = zf.testzip()
            check("zipfile: every entry passes its own CRC", bad is None, f"first bad: {bad}")
            names = zf.namelist()
            check(
                "zipfile: the plugin tree is present",
                any(n.endswith("package.json") for n in names)
                and any(n.endswith("index.ts") for n in names),
                f"names: {names}",
            )
            check(
                "zipfile: the hidden .env.example survived",
                any(n.endswith(".env.example") for n in names),
                f"names: {names}",
            )
            manifest = zf.read(next(n for n in names if n.endswith("package.json")))
            check(
                "zipfile: package.json parses as JSON",
                json.loads(manifest).get("name") == "plugin-probe",
                manifest[:200].decode("utf-8", "replace"),
            )
            with zf.open(next(n for n in names if n.endswith("index.ts"))) as handle:
                body = handle.read()
            check("zipfile: index.ts is not empty", len(body) > 0, f"{len(body)} bytes")
    except zipfile.BadZipFile as err:
        check("zipfile: the archive opens at all", False, str(err))

    print("\n[unzip] a second independent reader")
    if shutil.which("unzip") is None:
        print("  skip unzip is not installed")
    else:
        test = subprocess.run(
            ["unzip", "-t", str(archive_path)], capture_output=True, text=True
        )
        check("unzip -t: the archive tests clean", test.returncode == 0, test.stdout[-300:])
        dest = out / "extracted"
        dest.mkdir(exist_ok=True)
        got = subprocess.run(
            ["unzip", "-q", "-o", str(archive_path), "-d", str(dest)],
            capture_output=True,
            text=True,
        )
        check("unzip: the archive extracts", got.returncode == 0, got.stderr[-300:])
        extracted = sorted(
            str(p.relative_to(dest)) for p in dest.rglob("*") if p.is_file()
        )
        check(
            "unzip: the .env.example landed on disk",
            any(p.endswith(".env.example") for p in extracted),
            f"extracted: {extracted}",
        )

    print("\n[structure] what the bytes claim about themselves")
    if len(raw) < 22:
        check("the archive is at least an end-of-central-directory record", False, f"{len(raw)} bytes")
    else:
        check("the archive is at least an end-of-central-directory record", True)
        check(
            "the end record is the last 22 bytes",
            raw[-22:-18] == b"PK\x05\x06",
            repr(raw[-22:-18]),
        )

    if not args.keep:
        shutil.rmtree(out, ignore_errors=True)

    print(f"\n{checks - len(failures)}/{checks} checks passed")
    if failures:
        print("FAILED: " + ", ".join(failures))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
