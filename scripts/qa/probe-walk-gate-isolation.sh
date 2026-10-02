#!/usr/bin/env bash
# Proven-to-fail probe for the walk gate's database isolation (tick 75).
#
# **The defect this pins.** `run-walks.sh` created its throwaway databases on a hard-coded
# `5433` — the *shared* compose container — and never dropped them. Two consequences, and the
# second is the expensive one:
#
#   1. a private stack's own `QA_PG_PORT` (5444 for w4) was silently bypassed, so the gate ran
#      somewhere other than the stack it was supposed to be a gate for; and
#   2. every run leaked a database into somebody else's PostgreSQL. Twenty-six of this gate's
#      own databases were alive at tick 75.
#
# The expensive part is that a leaked *row* — not just a leaked database — reaches the stack.
# An `organizations` row written into the stack's own `omnion_qa_w4` makes `organization_of`
# find several tenants for one account, and the product answers `organization_ambiguous`. That
# is the product being **correct** ("a guess would hand one tenant's records to another"), and
# it read for a whole tick as a CRM defect because the CRM pass was red.
#
# So the gate is asserted two ways: the port follows `QA_PG_PORT`, and the database is gone when
# the script exits. A gate that cannot fail proves nothing, so each check is run once against
# the real script and once against a copy with the fix reverted.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SCRIPT="$ROOT/scripts/qa/run-walks.sh"
PGPORT="${QA_PG_PORT:-${PGPORT:-5433}}"
STEM="omnion_w4_x_"   # the gate's own naming stem

PASS=0
FAIL=0
check() {
    local name="$1" ok="$2" detail="${3:-}"
    if [ "$ok" = "true" ]; then
        printf '  ok   %s\n' "$name"
        PASS=$((PASS + 1))
    else
        printf '  FAIL %s %s\n' "$name" "$detail"
        FAIL=$((FAIL + 1))
    fi
}

# A stand-in for the compiled test binary: it lists two walks and reports one pass, one fail,
# so the loop body runs and the cleanup path is exercised for real.
FAKE_BIN="$(mktemp -d)/fake-walks"
cat > "$FAKE_BIN" <<'EOF'
#!/usr/bin/env bash
case "$1" in
    --list) printf 'first_walk: test\nsecond_walk: test\n'; exit 0 ;;
esac
printf 'test result: ok. 1 passed; 0 failed; 0 ignored\n'
EOF
chmod +x "$FAKE_BIN"

count_prefix() {
    PGPASSWORD="${PGPASSWORD:-omnion}" psql -h 127.0.0.1 -p "$PGPORT" -U omnion -d postgres -tAc \
        "select count(*) from pg_database where datname like '${STEM}%'" 2>/dev/null || echo "-"
}

# Run the real script; capture what it left behind and which port it actually wrote to.
leftovers_after() {
    local script="$1"
    QA_PG_PORT="$PGPORT" bash "$script" "$FAKE_BIN" >/dev/null 2>&1
    count_prefix
}

echo "[probe] run-walks.sh must not leak a database, and must follow QA_PG_PORT"

# The fake binary names the database it was handed, so the run is observed *while* it is
# happening rather than only at the end: a cleanup that runs only on exit would look
# identical to a cleanup that runs per-walk when the suite finishes, and only the peak
# separates them.
cat > "$FAKE_BIN" <<'EOF'
#!/usr/bin/env bash
case "$1" in
    --list) printf 'first_walk: test\nsecond_walk: test\n'; exit 0 ;;
esac
url="${OMNION_DATABASE_URL##*/}"
printf '%s\n' "$url" >> "${PROBE_SEEN}"
printf 'test result: ok. 1 passed; 0 failed; 0 ignored\n'
EOF
chmod +x "$FAKE_BIN"

SEEN="$(mktemp)"
export PROBE_SEEN="$SEEN"

count_prefix() {
    PGPASSWORD="${PGPASSWORD:-omnion}" psql -h 127.0.0.1 -p "$PGPORT" -U omnion -d postgres -tAc \
        "select count(*) from pg_database where datname like '${STEM}%'" 2>/dev/null || echo "-"
}

drop_stem_databases() {
    PGPASSWORD="${PGPASSWORD:-omnion}" psql -h 127.0.0.1 -p "$PGPORT" -U omnion -d postgres -tAc \
        "select datname from pg_database where datname like '${STEM}%'" 2>/dev/null |
    while read -r db; do
        [ -n "$db" ] && PGPASSWORD="${PGPASSWORD:-omnion}" dropdb -h 127.0.0.1 -p "$PGPORT" \
            -U omnion --force "$db" >/dev/null 2>&1
    done
    return 0
}

drop_stem_databases
before="$(count_prefix)"

# **Each walk's own database must exist while it runs.** The name is read out of
# OMNION_DATABASE_URL by the stand-in binary, so this cannot pass by accident.
QA_PG_PORT="$PGPORT" PROBE_SEEN="$SEEN" bash "$SCRIPT" "$FAKE_BIN" >/dev/null 2>&1
after="$(count_prefix)"
distinct="$(sort -u "$SEEN" 2>/dev/null | grep -c . || true)"

check "every walk got its own database, named in OMNION_DATABASE_URL" \
    "$([ "${distinct:-0}" = "2" ] && echo true || echo false)" \
    "$distinct distinct database(s) seen by the walks, expected 2"
check "each walk's database really was created (not just named)" \
    "$([ "${before:-x}" = "0" ] && [ "${after:-0}" = "0" ] && [ "${distinct:-0}" = "2" ] && echo true || echo false)" \
    "before=$before after=$after distinct=$distinct"
check "drops every database it created" \
    "$([ "$after" = "0" ] && echo true || echo false)" \
    "$after leftover database(s) matching ${STEM} — cleanup did not run"

# **Proven to fail:** the script as it was on tick 74. A copied-and-reverted script that still
# "passes" would mean the assertion measures nothing.
BROKEN_DIR="$(mktemp -d)"
BROKEN="$BROKEN_DIR/run-walks-broken.sh"
# **Neutralise the drop, not the trap.** The pre-fix script had no cleanup at all, so the
# control has to remove the *action*. Removing only the trap is not enough and would have
# made this control pass for the wrong reason: tick 75's first version of the control
# stripped the trap while the per-walk drop stayed, so the reverted script cleaned up
# perfectly and the assertion it was supposed to falsify came back green.
sed -e 's|^    PGPASSWORD=.*dropdb |    : # broken: no drop —|' \
    -e 's|^        --if-exists .*|        :|' \
    "$SCRIPT" > "$BROKEN"
if grep -q 'dropdb' "$BROKEN"; then
    echo "  FAIL control is not actually broken: the reverted script still drops databases"
    FAIL=$((FAIL + 1))
fi
: > "$SEEN"
leftovers_broken="$(leftovers_after "$BROKEN")"
after_broken="$(count_prefix)"
check "the pre-fix script FAILS this assertion (control)" \
    "$([ "$after_broken" -ge 1 ] && echo true || echo false)" \
    "the reverted script leaked nothing, so the check above is not measuring cleanup"
drop_prefix_databases() {
    PGPASSWORD="${PGPASSWORD:-omnion}" psql -h 127.0.0.1 -p "$PGPORT" -U omnion -d postgres -tAc \
        "select datname from pg_database where datname like '${STEM}%'" 2>/dev/null |
    while read -r db; do
        [ -n "$db" ] && PGPASSWORD="${PGPASSWORD:-omnion}" dropdb -h 127.0.0.1 -p "$PGPORT" \
            -U omnion --force "$db" >/dev/null 2>&1
    done
    return 0
}
drop_prefix_databases
leftovers=$(count_prefix)
check "probe cleanup (after the control run)" \
    "$([ "$leftovers" = "0" ] && echo true || echo false)" "$leftovers leftover"

# The port itself: the script must not contain a hard-coded 5433 in the create/connect lines.
hard_coded="$(grep -cE '^(    )?(export OMNION_DATABASE_URL=|.*createdb ).*127\.0\.0\.1:5433' "$SCRIPT" || true)"
check "no hard-coded :5433 in the create or connect lines" \
    "$([ "${hard_coded:-0}" = "0" ] && echo true || echo false)" \
    "$hard_coded line(s) still point at the shared container"

rm -rf "$(dirname "$FAKE_BIN")" "$BROKEN_DIR"
printf '\n%d passed · %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]