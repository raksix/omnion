#!/usr/bin/env python3
"""REQ-129 slice 3 — a fixture application that serves live traffic through a recipe.

## Why this file exists instead of a `psql` loop

The zero-downtime claim is about what an operator SEES while the recipe runs, so the
thing under measurement has to be a server genuinely serving requests the whole time.
A shell loop has no request log, so "no failed request throughout" has nothing to be
asserted against and the claim silently degrades into "the migration applied".

This is a real HTTP server with three release versions, and the difference between them
IS the recipe:

| mode | read shape | write shape |
| --- | --- | --- |
| `v1` | old column only | old column only |
| `v2` | `coalesce(new, old)` — dual read | writes both — dual write |
| `v2-naive` | the new column alone | writes both |
| `v3` | the new column alone, after the constraint | writes both |

`v2-naive` is the defect the harness must be able to CATCH. It is the ordinary mistake:
cut over to the new column as soon as it exists, without the fallback. During the
backfill window it serves NULL for every row the backfill has not reached, which is
exactly the outage the recipe prevents. A harness that passed `v2-naive` would be
measuring nothing, so the proof RUNS it and requires the harness to red.

## Why `psql` subprocesses and not a driver

There is no Postgres driver installed for Python on this box, and the proof must not
depend on one — a proof that needs a package nobody installed is a proof that does not
run. `psql` is already required by every other script in `scripts/qa/`, it is on PATH,
and it speaks the same SQL. The cost is a subprocess per query, which is irrelevant
here: the measurement is about the RECIPE, not about throughput, and the request log
records outcomes rather than latency.

One thing this costs is worth stating: a subprocess has no persistent session, so
transaction state cannot be assumed to survive between calls. Every helper below
therefore either runs a single statement or wraps itself in `BEGIN … COMMIT`.

## The request log is the measurement surface

Every request appends one line, whatever its outcome:

    <iso8601> <thread> ok|error <path> detail

Nothing is summarised. "No failed request throughout" is then a `grep -c`, not a
self-report from the process that was supposed to be serving. A line is written on the
failure path too — that is the only reason the claim is falsifiable, because a `psql`
error that escaped would otherwise leave no trace.

Writes are on purpose. A traffic generator that only SELECTs cannot make the constraint
migration wait for anybody, so it would prove nothing about lock contention: the recipe
would look zero-downtime because the fixture had no locks to contend with. Each write
takes a short transaction on the same table the later DDL needs, which is what makes
`lock_timeout` a real bound rather than decoration.
"""

from __future__ import annotations

import argparse
import datetime as dt
import os
import signal
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# The window between "the column exists" and "the backfill reached this row" is the
# whole reason dual read exists. It is written down so the harness can assert it
# happened rather than assume the backfill was fast enough to hide it.
NEW_COLUMN = "display_total_cents"
OLD_COLUMN = "total_cents"

MODES = ("v1", "v2", "v2-naive", "v3")

# The value the log carries when a column is NULL. It is produced by a SQL `case`, not by
# casting: `null::text` renders as an EMPTY field, so a handler comparing against the
# string "NULL" would never see it. See `Store.read_one`.
NULL_SENTINEL = "__NULL__"


def SERVED_EXPR(column: str) -> str:  # noqa: N802 - a SQL fragment builder, called in caps
    """SQL that renders a column as text, or the sentinel when it is NULL."""
    return f"case when {column} is null then '{NULL_SENTINEL}' else {column}::text end"


def now_iso() -> str:
    return dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")


def psql(dsn: str, sql: str) -> tuple[int, str]:
    """Run one statement set. Returns (rc, output).

    `ON_ERROR_STOP=1` matters: without it psql answers rc=0 after a failed statement
    and the caller cannot tell success from failure unless it parses text. Every
    helper here branches on rc, so it must be the rc that is truthful.
    """
    proc = subprocess.run(
        ["psql", dsn, "-X", "-q", "-v", "ON_ERROR_STOP=1", "-tAF", "|", "-c", sql],
        capture_output=True,
        text=True,
    )
    return proc.returncode, (proc.stdout or "").strip()


class Store:
    def __init__(self, dsn: str, mode: str, log_path: str) -> None:
        self.dsn = dsn
        self.mode = mode
        self.log_path = log_path
        self.log_lock = threading.Lock()
        self.counter_lock = threading.Lock()
        self.reads = 0
        self.writes = 0
        self.errors = 0
        self.write_cursor = 0

    def log(self, outcome: str, path: str, detail: str = "") -> None:
        # Write and flush under a lock: the proof greps this file while the server is
        # still running, so a buffered line would be a missing measurement.
        with self.log_lock:
            with open(self.log_path, "a", encoding="utf-8") as handle:
                handle.write(
                    f"{now_iso()} {threading.current_thread().name} "
                    f"{outcome} {path} {detail}\n"
                )
                handle.flush()

    def count(self, ok: bool, write: bool = False) -> None:
        with self.counter_lock:
            if not ok:
                self.errors += 1
            elif write:
                self.writes += 1
            else:
                self.reads += 1

    def read_one(self, row_id: int) -> tuple[int, str]:
        """Return (http_status, detail) for one order.

        The detail string carries `served` and `new` so the harness can tell a
        SUCCESSFUL response from a CORRECT one. A server answering 200 with a NULL
        total is a failed migration wearing a success status, and no HTTP-level check
        can see it — the harness greps for `NULL_TOTAL`.

        The two modes that differ ONLY in the fallback clause, which is the whole point:

            v2         select coalesce(new, old), new  -- serves old until backfilled
            v2-naive   select new,                 new  -- serves NULL until backfilled

        Both answer 200. That is why the log line carries `served`/`new` and not just a
        status: a harness that only checked the status code would pass the naive
        cut-over, which is exactly the mistake the recipe exists to prevent.

        ## Why the sentinel is built in SQL with `case`, and not in Python

        An earlier version selected `col::text` and compared the result to the string
        `"NULL"` here in the handler. PostgreSQL renders a NULL `::text` as an EMPTY
        field, so that comparison never fired: with the fallback removed by mutation the
        fixture logged `served=` and the "no NULL total" assertion STILL PASSED. The
        detector was measuring a rendering convention rather than the value.

        `case when col is null then …` distinguishes the two cases at the source, so
        there is nothing left to infer from formatting. An EMPTY `served` is treated as
        a failure as well, because an empty total is as wrong as a null one and no future
        column type is obliged to render a NULL as the sentinel.
        """
        if self.mode == "v1":
            # v1 runs BEFORE the recipe adds the new column, so it must not name it —
            # a reference to a column that does not exist yet is a 500, not a NULL. The
            # second field is therefore a LITERAL sentinel rather than a column: "there
            # is no new column yet" and "the new column exists and is NULL" are the same
            # state from the caller's side, and both mean the old value is authoritative.
            rc, out = psql(
                self.dsn,
                f"select {SERVED_EXPR(OLD_COLUMN)}, '{NULL_SENTINEL}' "
                f"from orders where id = {row_id}",
            )
            via = "none"
        elif self.mode in ("v2-naive", "v3"):
            rc, out = psql(
                self.dsn,
                f"select {SERVED_EXPR(NEW_COLUMN)}, {SERVED_EXPR(NEW_COLUMN)} "
                f"from orders where id = {row_id}",
            )
            via = NEW_COLUMN
        else:  # v2 — coalesce(new, old) IS the fallback
            rc, out = psql(
                self.dsn,
                f"select {SERVED_EXPR(f'coalesce({NEW_COLUMN}, {OLD_COLUMN})')}, "
                f"{SERVED_EXPR(NEW_COLUMN)} "
                f"from orders where id = {row_id}",
            )
            via = f"coalesce({NEW_COLUMN},{OLD_COLUMN})"
        if rc != 0 or not out:
            return 500, f"query_failed rc={rc}"
        parts = out.split("|")
        served_raw = parts[0]
        new_raw = parts[1] if len(parts) > 1 else NULL_SENTINEL
        return 200, f"served={served_raw} new={new_raw} via={via}"

    def write_one(self) -> tuple[bool, str]:
        with self.counter_lock:
            self.write_cursor += 1
            seq = self.write_cursor
        amount = 1000 + (seq % 500)
        if self.mode == "v1":
            sql = (
                f"begin; insert into orders ({OLD_COLUMN}, note) "
                f"values ({amount}, 'v1 write {seq}'); commit;"
            )
        else:
            # Dual write: the new column is written from the moment the new code is
            # deployed, which is what lets the backfill be a bounded catch-up rather
            # than an open-ended race against live inserts.
            sql = (
                f"begin; insert into orders ({OLD_COLUMN}, {NEW_COLUMN}, note) "
                f"values ({amount}, {amount}, 'v2 write {seq}'); commit;"
            )
        rc, out = psql(self.dsn, sql)
        return rc == 0, f"rc={rc} {out}"[:120]

    def handle(self, path: str) -> tuple[int, str]:
        if path.startswith("/order/"):
            row_id = int(path.rsplit("/", 1)[1])
            status, detail = self.read_one(row_id)
            if status != 200:
                self.count(False)
                self.log("error", path, detail)
                return status, detail
            served = detail.split("served=", 1)[1].split()[0]
            # Both spellings of "no value" are failures, and the EMPTY one matters as
            # much as the sentinel: an empty total is as wrong as a null one, and it is
            # exactly what `null::text` renders as — which is how the first version of
            # this check missed a removed fallback.
            if served in (NULL_SENTINEL, "", "None", "null"):
                # 200 with a NULL total: logged as an ERROR on purpose. This is the
                # failure the recipe exists to prevent and it is invisible to a status
                # code check, so the log has to carry the verdict, not the protocol.
                self.count(False)
                self.log("error", path, f"NULL_TOTAL {detail}")
                return 200, detail
            self.count(True)
            self.log("ok", path, detail)
            return 200, detail

        if path == "/write":
            ok, detail = self.write_one()
            self.count(ok, write=True)
            self.log("ok" if ok else "error", path, f"write {detail}")
            return (200 if ok else 500), detail

        if path == "/counters":
            with self.counter_lock:
                return 200, (
                    f"reads={self.reads} writes={self.writes} errors={self.errors}"
                )

        self.log("error", path, "no route")
        return 404, "no route"


def make_handler(store: Store):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler's contract
            try:
                status, body = store.handle(self.path)
            except Exception as exc:  # noqa: BLE001 - record ANY failure
                store.count(False)
                store.log("error", self.path, f"EXC {type(exc).__name__} {exc}")
                status, body = 500, "exception"
            payload = body.encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def log_message(self, *_args, **_kwargs) -> None:
            # Access logging would interleave on stderr; the request log is the
            # record that counts.
            return

    return Handler


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--dsn", required=True)
    parser.add_argument("--mode", required=True, choices=list(MODES))
    parser.add_argument("--log", required=True)
    parser.add_argument("--port", type=int, required=True)
    args = parser.parse_args()

    # Truncate: appending to a stale log would carry a previous phase's failures into
    # the next phase's count, which is the easiest way to make a green run meaningless.
    with open(args.log, "w", encoding="utf-8"):
        pass

    store = Store(args.dsn, args.mode, args.log)
    server = ThreadingHTTPServer(("127.0.0.1", args.port), make_handler(store))
    server.daemon_threads = True

    def shutdown(_signum, _frame):
        threading.Thread(target=server.shutdown, daemon=True).start()

    signal.signal(signal.SIGTERM, shutdown)
    signal.signal(signal.SIGINT, shutdown)
    print(f"ready mode={args.mode} port={args.port}", flush=True)
    server.serve_forever()
    return 0


if __name__ == "__main__":
    sys.exit(main())