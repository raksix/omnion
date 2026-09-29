#!/usr/bin/env python3
"""Run the reports walks against this stack's own QA database.

The password is read out of `scripts/qa/run.sh` **without printing it**: the
credential mask in tool output means the value never reaches the agent's
context, which is the whole reason this reads the file here instead of being
handed a pasted URL on the command line.

`OMNION_REQUIRE_DB=1` is not optional. These walks' subject is numbers — a
value, a net movement, a count beside a table — and a suite that SKIPs when
PostgreSQL is down is a green tick that proved nothing, which is the exact
failure this module's slice 5 hit twice.

Usage: run-walks.py [extra cargo test args]
"""
import os
import re
import subprocess
import sys
from pathlib import Path
from urllib.parse import urlparse, urlunparse

ROOT = Path(__file__).resolve().parents[2]
RUN_SH = ROOT / "scripts" / "qa" / "run.sh"
STACK = os.environ.get("QA_STACK", "w4")
# **The walks get their own database, deliberately.** The browser pass owns
# `omnion_qa_<stack>` and resets it at the start of every run, so a walk suite pointed
# at it fails with `database "omnion_qa_w4" does not exist` roughly once per pass — and
# because the failure reads like a permission or a migration problem, it has already
# cost this suite two ticks of misdiagnosis. One suite per database is the rule the
# ledger learned the expensive way; the fix is to stop sharing.
#
# The database is created here if it is missing, and `live_state` applies every
# migration, so a fresh checkout needs nothing prepared.
DB = os.environ.get("QA_WALK_DB", f"{'omnion_qa' if STACK == 'main' else f'omnion_qa_{STACK}'}_walks")


def database_url() -> str:
    text = RUN_SH.read_text(encoding="utf-8")
    match = re.search(
        r'OMNION_DATABASE_URL="postgres://([^:]+):([^@"]+)@([^:]+):(\d+)/\$\{?QA_DB', text
    )
    if not match:
        print("could not read the QA database URL out of scripts/qa/run.sh", file=sys.stderr)
        raise SystemExit(1)
    user, password, host, port = match.groups()
    return f"postgres://{user}:{password}@{host}:{port}/{DB}"


def ensure_database(url: str) -> None:
    """Create the walk database if this is the first run against it.

    Through `psql` and the maintenance database, **not** through a Python driver: the
    first version of this used `psycopg`, which is not installed here, and the suite it
    was supposed to prepare then failed with `database "..." does not exist` — the
    exact symptom the function exists to remove. A preparation step that silently does
    nothing because its own import is missing is worse than no preparation step, because
    it is indistinguishable from a database problem.

    `createdb` (or a `psql -c "create database"`) is the tool that is actually on this
    box, and the suite's own `live_state` applies every migration afterwards, so a fresh
    checkout needs nothing prepared by hand.
    """
    parsed = urlparse(url)
    name = parsed.path.lstrip("/")
    if not name:
        return
    admin = urlunparse(parsed._replace(path="/postgres"))
    admin_env = dict(os.environ, PGPASSWORD=parsed.password or "")
    exists = subprocess.run(
        ["psql", admin, "-tAc", f"select 1 from pg_database where datname = '{name}'"],
        env=admin_env, capture_output=True, text=True,
    )
    if exists.returncode != 0:
        print(f"[walks] cannot reach the maintenance database: {exists.stderr.strip()}",
              flush=True)
        return
    if exists.stdout.strip() == "1":
        return
    created = subprocess.run(
        ["psql", admin, "-tAc", f'create database "{name}"'],
        env=admin_env, capture_output=True, text=True,
    )
    if created.returncode == 0:
        print(f"[walks] created {name}", flush=True)
    else:
        print(f"[walks] could not create {name}: {created.stderr.strip()}", flush=True)


def main(argv: list[str]) -> int:
    env = dict(os.environ)
    env["PATH"] = f"{Path.home() / '.cargo' / 'bin'}:{env.get('PATH', '')}"
    env["OMNION_DATABASE_URL"] = database_url()
    ensure_database(env["OMNION_DATABASE_URL"])
    env.setdefault("OMNION_REDIS_URL", "redis://127.0.0.1:6380")
    env["OMNION_REQUIRE_DB"] = "1"
    # The CSRF layer refuses every cookie-authenticated mutation without a secret,
    # which would turn each write walk into an assertion about a missing
    # deployment key rather than about the behaviour it is here to prove.
    env.setdefault("OMNION_CSRF_SECRET", "inventory-reports-suite-csrf-secret")

    args = argv or ["--test-threads=1"]
    print(f"[walks] stack {STACK} · database {DB} · args: {' '.join(args)}", flush=True)
    return subprocess.run(
        ["cargo", "test", "-p", "omnion-api", "--test", "inventory_reports", "--", *args],
        cwd=ROOT,
        env=env,
    ).returncode


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
