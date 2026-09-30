#!/usr/bin/env bash
# Run one integration walk per process against its own fresh database (REQ-054).
#
# **Why not `cargo test -- --test-threads=1`:** the suite serialises on a static mutex, so the
# first failing walk makes every walk behind it read as "blocked" — a statement about the queue,
# not about the walk. Tick 40 spent a tick diagnosing a walk that passes alone in ten seconds.
#
# **And why the databases are created here:** `live_state()` answers `None` when PostgreSQL is
# unreachable and the test then *returns*, which libtest reports as `ok`. A whole suite can be
# green because nothing ran at all — the exact shape of a false green, and the reason this script
# counts what it ran instead of trusting the exit code.
set -uo pipefail

BINARY="$1"
shift

PASS=0
FAIL=0
SKIPPED=0

for name in $("$BINARY" --list 2>/dev/null | grep ': test$' | sed 's/: test//'); do
    # **A fresh name every run, never a reused one.** sqlx records each migration's checksum, so a
    # database that already applied version 179 answers `VersionMismatch(179)` the moment that file
    # is edited — which is the normal case while a migration is being written, and it looks
    # identical to "the schema is wrong". Dropping and recreating is the only way to be sure the
    # walk ran against what the file says right now. The suffix is the build's own timestamp, so
    # two runs a second apart still get separate databases.
    slug="$(printf '%s' "$name" | md5sum | cut -c1-8)"
    database="omnion_w4_x_${slug}_$(date +%s)_$$"

    PGPASSWORD=omnion createdb -h 127.0.0.1 -p 5433 -U omnion "$database" 2>/dev/null || true

    export OMNION_DATABASE_URL="postgres://omnion:omnion@127.0.0.1:5433/${database}"
    # **300 seconds, and the reason is stated here rather than discovered.** Under the box's
    # normal load (a dozen writers, load average 100+) a walk that migrates a fresh database takes
    # well over two minutes, and `timeout 120` killed all eleven at once — which the runner
    # reported as eleven "NO RESULT LINE"s. A timeout that fires under load is indistinguishable
    # from a hang, so the budget has to cover the slow case rather than the average one.
    output="$(timeout 300 "$BINARY" --exact "$name" --test-threads=1 2>&1)"
    # **Parse on `result: ok` / `result: FAILED`**, which are the two words libtest actually prints
    # — not on a pattern that happens to match one of them. The first version of this script grepped
    # for `test result: [a-z]*\.` and therefore matched neither `ok.` nor `FAILED.`, so eleven real
    # failures printed as "NO RESULT" and the summary read `0 passed`. A verifier that cannot read
    # its own harness is worse than no verifier: it reports a number nobody checks.
    summary="$(printf '%s' "$output" | grep '^test result:' | tail -1)"
    ran="$(printf '%s' "$summary" | sed -n 's/.*[^0-9]\([0-9][0-9]*\) passed.*/\1/p')"
    failed="$(printf '%s' "$summary" | sed -n 's/.*[^0-9]\([0-9][0-9]*\) failed.*/\1/p')"
    failed="${failed:-0}"

    if [ -z "$ran" ]; then
        verdict="NO RESULT LINE"
        SKIPPED=$((SKIPPED + 1))
    elif [ "$ran" -ge 1 ] && [ "$failed" -eq 0 ]; then
        verdict="PASS ($ran)"
        PASS=$((PASS + 1))
    elif [ "$ran" -eq 0 ] && [ "$failed" -eq 0 ]; then
        verdict="SKIPPED — the harness returned without running anything"
        SKIPPED=$((SKIPPED + 1))
    else
        verdict="FAIL ($failed failed)"
        FAIL=$((FAIL + 1))
        printf '%s\n' "$output" | grep -A 6 'panicked at' | head -20
    fi
    printf '[%-9s] %s\n' "$verdict" "$name"
done

printf '\n%d passed · %d failed · %d skipped\n' "$PASS" "$FAIL" "$SKIPPED"
[ "$FAIL" -eq 0 ] && [ "$SKIPPED" -eq 0 ]
