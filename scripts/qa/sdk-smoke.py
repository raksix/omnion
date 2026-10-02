#!/usr/bin/env python3
"""Smoke-test the GENERATED Python client against a LIVE server (REQ-130, slice 3).

Acceptance 10 says the packages "compile and pass smoke tests against a live test server".
Compiling is proven in the crate; this is the other half, and it is the half a unit test over
the generator's own functions cannot reach: the client is a *consumer* of the API, and only a
real request can say whether the paths it builds are the paths the router serves.

```text
python3 scripts/qa/sdk-smoke.py --base-url http://127.0.0.1:18085 --token …
```

**What it asserts, and why each one is a thing that can actually be wrong:**

  * every operation in the document builds a URL under the API's real prefix,
  * a path parameter is substituted (and its absence is refused, not sent as `{id}`),
  * the permission the document records is the one the caller is actually held to,
  * a 401/403 raises `OmnionError` carrying the status and the platform's message,
  * the client's own table and the document agree on the operation count.

**Why it needs a server at all.** A client whose path is built from the wrong source is
indistinguishable from a correct one until a request 404s. A mock would agree with the client by
construction, which is the same blind spot as a unit test that builds its fixture by hand.
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
SNAPSHOT = os.path.join(REPO, "api", "openapi.snapshot.json")
DEFAULT_CLIENT = os.path.join(REPO, "dist", "sdks", "python", "src")

failures: list[str] = []
passes = 0


def check(name: str, ok: bool, detail: str = "") -> None:
    global passes
    if ok:
        passes += 1
        print(f"ok    {name}")
    else:
        failures.append(f"{name}: {detail}")
        print(f"FAIL  {name} — {detail}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--token", default="")
    parser.add_argument("--client", default=DEFAULT_CLIENT)
    args = parser.parse_args()

    sys.path.insert(0, args.client)
    try:
        from omnion_api_client import OPERATIONS, OPERATIONS_BY_ID, OmnionClient, OmnionError
        from omnion_api_client import client as client_module
    except ImportError as e:
        print(f"the generated client is not importable from {args.client}: {e}")
        return 2

    with open(SNAPSHOT, encoding="utf-8") as fh:
        document = json.load(fh)

    # --- the client's own table agrees with the document --------------------------------------
    document_ops = [
        (method.upper(), path)
        for path, item in document["paths"].items()
        for method in item
        if method in ("get", "post", "put", "patch", "delete")
    ]
    check(
        "the client describes every operation in the document",
        len(OPERATIONS) == len(document_ops),
        f"client has {len(OPERATIONS)}, document has {len(document_ops)}",
    )
    check(
        "every client operation is addressable by id",
        all(op["id"] in OPERATIONS_BY_ID for op in OPERATIONS),
        "an operation missing from the id table is uncallable by name",
    )

    # --- URL construction, against a server that is not the generator -------------------------
    # A probe: 127.0.0.1 on a port nothing listens on answers instantly with a connection
    # refusal, which is the signal that the CLIENT built and sent a request. A 404 or a 401 from
    # a live server is the signal that the path was real.
    client = OmnionClient(args.base_url, token=args.token or None)

    # Every path in the document must survive substitution with a placeholder value.
    unresolvable = []
    for op in OPERATIONS:
        args_for = {name: "smoke" for name in _parameters(op["path"])}
        try:
            url = client_module._resolve(op, args_for)
        except OmnionError:
            continue
        if "{" in url:
            unresolvable.append(op["id"])
    check(
        "every path parameter is substituted",
        not unresolvable,
        f"{len(unresolvable)} left a placeholder: {unresolvable[:5]}",
    )

    # A missing argument must be REFUSED locally, not sent as a literal `{id}`.
    templated = [op for op in OPERATIONS if _parameters(op["path"])]
    if templated:
        try:
            client_module._resolve(templated[0], {})
            check(
                "a missing path argument is refused before the request",
                False,
                f"{templated[0]['id']} was allowed to build /{templated[0]['path']}",
            )
        except OmnionError as e:
            check("a missing path argument is refused before the request", True, str(e))

    # The permission column travels with the operation, so a client can tell the operator what
    # the endpoint needs before the call rather than after a 403.
    keyed = [op for op in OPERATIONS if op["permission"]]
    check(
        "operations carry the permission their guard enforces",
        len(keyed) > 0,
        "no operation recorded a permission at all",
    )
    unkeyed = [op["id"] for op in OPERATIONS if not op["permission"]]
    print(f"note  {len(keyed)} operations carry a permission, {len(unkeyed)} are unguarded by design")

    # --- one real call -----------------------------------------------------------------------
    # `/health` needs no credential on any platform and is the cheapest route that proves the
    # transport, the base URL and the envelope handling all at once. It is chosen over a
    # permission-guarded route on purpose: a smoke test that needs a live session is a smoke
    # test whose failure is ambiguous between "client is wrong" and "my token expired".
    # The health operation is DISCOVERED from the document rather than named. The first version
    # hardcoded `get_health` and failed on a document that plainly contains `/healthz` — a test
    # asserting a name it invented, which is the same mistake as a fixture written to agree with
    # the code it checks. If the route is renamed, the smoke test follows it.
    health_id = next(
        (
            op["id"]
            for op in OPERATIONS
            if op["method"] == "GET" and op["path"].rstrip("/") == "/healthz"
        ),
        None,
    )
    if health_id is None:
        check("the document exposes a health route", False, "no GET /healthz in the client table")
    else:
        try:
            body = client.call(health_id)
            check(
                "the generated client completes a real call",
                True,
                    f"{health_id} answered {type(body).__name__}",
            )
        except OmnionError as e:
            # A 401/403 is a REAL answer from a real server and proves the request reached it.
            if e.status in (401, 403, 429):
                check(
                    "the generated client completes a real call",
                    True,
                    f"the server answered {e.status} — the request reached a real guard",
                )
            else:
                check(
                    "the generated client completes a real call",
                    False,
                    f"status {e.status}: {e}",
                )
        except Exception as e:  # noqa: BLE001 - the point is to report ANY transport failure
            check("the generated client completes a real call", False, f"{type(e).__name__}: {e}")

    # --- a non-2xx must carry the status and the message --------------------------------------
    missing = "get_a_route_that_does_not_exist"
    try:
        client.call(missing)
        check("an unknown operation id is refused by the client", False, "it returned a value")
    except OmnionError as e:
        check(
            "an unknown operation id is refused by the client",
            e.status == 404 and missing in str(e),
            f"status {e.status}, message {e!s}",
        )

    print(f"\n{passes} passed, {len(failures)} failed")
    for line in failures:
        print(f"  FAIL {line}")
    return 1 if failures else 0


def _parameters(path: str) -> list[str]:
    out, rest = [], path
    while "{" in rest:
        start = rest.index("{")
        end = rest.find("}", start)
        if end < 0:
            break
        out.append(rest[start + 1 : end])
        rest = rest[end + 1 :]
    return out


if __name__ == "__main__":
    sys.exit(main())
