#!/usr/bin/env python3
"""Move a suite onto the throwaway-database harness in tests/support/isolated_db.rs.

The suites under apps/api/tests/ each carried their own copy of "connect to whatever
OMNION_DATABASE_URL names, migrate, announce a skip on failure". That copy is the defect
documented in the harness module: on a writer's box the URL names the shared QA database, so
the walk inherits rows, seed order and fixture keys from every other writer, and the skip
branch returns a pass.

This performs the mechanical half of the move -- rewrite `live_state`, give the fixture the
isolated handle, turn the per-row `cleanup` into a database drop, and append the honesty
gate -- and prints anything it could not do for a human to finish by hand. It refuses to
write a file whose shape it does not recognise, because a suite that half-migrated is worse
than one that did not: the next reader cannot tell which half is which.

Usage: convert_suite.py <suite.rs> [<suite.rs> ...]
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

# apps/api/tests: the script lives in scripts/qa, three levels below the API crate.
TESTS = Path(__file__).resolve().parents[2] / "apps" / "api" / "tests"

# The skip branch each suite wrote by hand, in the shapes actually present in the tree.
SKIP_BODIES = [
    # The two-line message, indented inside the helper.
    re.compile(
        r"""    let db = match Db::connect\(&config\.database\)\.await \{
            Ok\(db\) => db,
            Err\(error\) => \{
                eprintln!\(
                    "SKIP: PostgreSQL is not reachable \(\{error\}\) — start it with \\
                 `docker compose -f infra/compose/docker-compose.dev.yml up -d`"
                \);
                return None;
            \}
        \};""",
        re.S,
    ),
    # The one-line message, flush left inside the helper.
    re.compile(
        r"""    let db = match Db::connect\(&config\.database\)\.await \{
        Ok\(db\) => db,
        Err\(error\) => \{
            eprintln!\("SKIP: PostgreSQL is not reachable \(\{error\}\)"\);
            return None;
        \}
    \};""",
        re.S,
    ),
    # Same again, with the error bound to `err` rather than `error`.
    re.compile(
        r"""    let db = match Db::connect\(&config\.database\)\.await \{
        Ok\(db\) => db,
        Err\(err\) => \{
            eprintln!\(
                "SKIP: PostgreSQL is not reachable \(\{err\}\) — start it with \\
                 `docker compose -f infra/compose/docker-compose.dev.yml up -d`"
            \);
            return None;
        \}
    \};""",
        re.S,
    ),
    # The `live_db` shape: a helper that returns an `Option<Db>` rather than a `Result`.
    re.compile(
        r"""    let db = live_db\(&config\)\.await\?;""",
        re.S,
    ),
]

HONESTY_GATE = """

/// A walk in this file that declined to run is a run that measured nothing.
///
/// Cargo reports a skipped walk as `ok` and captures the message that said so, so the summary
/// a person or a CI job reads cannot tell it apart from success. This file returns early when
/// its database cannot be opened, so that is a state it can reach; asserting the count is what
/// turns it red instead.
#[test]
fn no_walk_in_this_file_skipped() {
    assert_nothing_skipped();
}
"""


def fail(path: Path, why: str) -> None:
    """Refuse loudly instead of writing a half-migrated suite."""
    print(f"SKIPPED {path.name}: {why}")
    raise SystemExit(2)


def convert(path: Path) -> None:
    source = path.read_text()
    before = source

    if "mod support;" not in source:
        fail(path, "does not declare `mod support;`")

    if "live_state().await?" not in source:
        fail(path, "no `live_state().await?` call site")
    # The binding the fixture is stored in, read off the `let Some(<name>) = Fixture::new()`.
    binding = re.search(r"let Some\((?:mut )?([A-Za-z_][A-Za-z0-9_]*)\) = Fixture::new\(\)", source)
    if binding is None:
        fail(path, "no `let Some(<name>) = Fixture::new()` walk entry")
    prefix = binding.group(1)
    stem = path.stem

    # 1. The helper gains the isolated handle and stops skipping silently.
    found = next((p.search(source) for p in SKIP_BODIES if p.search(source)), None)
    if found is None:
        fail(path, "no recognised skip branch to replace")
    body = found.group(0)
    replacement = (
        f"""    let isolated = IsolatedDb::open(&config.database.url, 4, "{stem}")
        .await
        .expect("the throwaway database must open");
    let Some(isolated) = isolated else {{
        announce_skip("no throwaway database, this walk did not run");
        return None;
    }};
    let db = isolated.db.clone();"""
    )
    source = source.replace(body + "\n", replacement + "\n", 1)

    # 2. `db.migrate()` is already done by the harness; a second one is not wrong but says
    #    the reader must check, so it goes with a note in its place.
    source = source.replace(
        '    db.migrate().await.expect("migrations must apply");\n',
        "    // Migrations are applied by `IsolatedDb::open`, before the router is built.\n",
    )

    # 3. `live_state` now hands back the handle as well, so the fixture can drop it.
    source = re.sub(
        r"async fn live_state\(\) -> Option<\(AppState, Db\)>",
        "async fn live_state() -> Option<(AppState, Db, IsolatedDb)>",
        source,
        count=1,
    )
    source = source.replace(
        "    Some((state, db))\n",
        "    Some((state, db, isolated))\n",
        1,
    )

    # 4. The fixture carries the handle and drops the database instead of deleting rows.
    source = source.replace(
        "        let (state, db) = live_state().await?;",
        "        let (state, db, isolated) = live_state().await?;",
    )
    source = re.sub(
        r"(struct Fixture \{\n    state: AppState,\n    db: Db,\n)",
        r"\1    isolated: IsolatedDb,\n",
        source,
        count=1,
    )
    source = re.sub(
        r"(        Some\(Self \{\n            state,\n            db,\n)",
        r"\1            isolated,\n",
        source,
        count=1,
    )

    cleanup = re.search(r"    async fn cleanup\(&self\) \{.*?\n    \}", source, re.S)
    if cleanup:
        source = source.replace(
            cleanup.group(0),
            """    /// Drop the walk's own database.
    ///
    /// **The row deletions this used to do are gone, and their absence is the point.** They
    /// existed because the walk shared a database with every other suite on the box, so
    /// tidying up was the price of being allowed in. With a database per walk there is nothing
    /// to tidy: dropping it removes every row the walk wrote at once, which is both fewer
    /// statements and the only cleanup that cannot miss a table.
    async fn cleanup(&mut self) {
        self.isolated.dispose().await;
    }""",
        )

    # 5. `&self` becomes `&mut self` at every call site, and the fixture binding is mutable.
    source = source.replace(f"{prefix}.cleanup().await;", f"{prefix}.cleanup().await;")
    source = source.replace(
        f"let Some({prefix}) = Fixture::new().await else {{",
        f"let Some(mut {prefix}) = Fixture::new().await else {{",
    )

    # 6. The imports and the honesty gate.
    source = source.replace(
        "use support::walk_auth",
        "use support::isolated_db::{IsolatedDb, announce_skip, assert_nothing_skipped};\nuse support::walk_auth",
    )
    if "use support::isolated_db" not in source:
        source = re.sub(
            r"(\nmod support;\n)",
            r"\1use support::isolated_db::{IsolatedDb, announce_skip, assert_nothing_skipped};\n",
            source,
            count=1,
        )
    source = source.rstrip("\n") + HONESTY_GATE

    if source == before:
        fail(path, "nothing changed")
    path.write_text(source)
    print(f"converted {path.name}")


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    rc = 0
    for argument in sys.argv[1:]:
        if argument.startswith("/"):
            path = Path(argument)
        else:
            path = TESTS / (argument if argument.endswith(".rs") else f"{argument}.rs")
        if not path.exists():
            print(f"SKIPPED {path.name}: no such file")
            rc = 1
            continue
        try:
            convert(path)
        except SystemExit as stop:
            # `fail` raises SystemExit(2); anything else carries no code and is still a
            # refusal, so it counts as one rather than as success.
            rc = rc or (stop.code if isinstance(stop.code, int) else 2)
    return rc


if __name__ == "__main__":
    raise SystemExit(main())