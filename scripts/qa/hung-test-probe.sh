#!/usr/bin/env bash
# Explain a *hung* Rust integration test in one command, so a tick does not spend its
# whole budget rediscovering what is already known.
#
# It exists because four consecutive ticks (67, 68, 69 and this one) each burned a run on
# re-deriving this from scratch, and two of them named the wrong variable: tick 68 blamed
# free space on /mnt/apopic, tick 69 retracted it, and neither of them took the two
# measurements that settle it. The retraction was correct and still cost a tick.
#
# What this prints, and what each answer means:
#
#   CPU delta over the window is 0        -> the process is not working. It is waiting on a
#                                           future nobody is going to wake, or on a lock.
#                                           `ps %CPU` cannot show this: it is one sample and
#                                           rounds to 0 for everything slower than a tick.
#   a thread in `futex_do_wait`          -> blocked on a lock or a condvar.
#   a worker in `do_epoll_wait`          -> the runtime is idle: it believes it has nothing
#                                           runnable. Paired with a zero CPU delta, that is a
#                                           lost wakeup or a lock, not slow I/O.
#   pg_stat_activity rows state <> idle  -> the database is genuinely busy; this row is the
#                                           query it is busy with. **Empty means the database
#                                           is not the reason**, however long the test waits.
#   blocked_clients > 0 on Redis         -> Redis is holding a connection (BLOCK/BRPOP family).
#                                           blocked_clients == 0 means Redis is not the reason.
#
# Usage:
#   bash scripts/qa/hung-test-probe.sh crm          # by test-binary prefix
#   bash scripts/qa/hung-test-probe.sh crm-8089c15  # full prefix works too
#
# It reads only /proc, pg_stat_activity and redis-cli. It sends no signal, so it is safe
# to run against somebody else's test process — including a sibling writer's.

set -uo pipefail

BINARY="${1:-}"
WINDOW="${WINDOW:-30}"
PG_PORT="${QA_PG_PORT:-5444}"
PG_DB="${QA_DB:-omnion_qa_w4}"
REDIS_PORT="${OMNION_REDIS_PORT:-6380}"

[ -n "$BINARY" ] || { echo "usage: $0 <test-binary-prefix> [window-seconds]" >&2; exit 2; }

find_pid() {
  local p
  for p in /proc/[0-9]*; do
    [ -r "$p/exe" ] || continue
    local exe
    exe="$(readlink "$p/exe" 2>/dev/null)" || continue
    case "$exe" in
      *"$BINARY"*) basename "$p"; return 0 ;;
    esac
  done
  return 1
}

PID="$(find_pid)" || { echo "no process matches '$BINARY' — it may have finished; check the log first" >&2; exit 1; }

cpu_of() {
  local stat rest
  stat="$(cat "/proc/$1/stat" 2>/dev/null)" || { echo "gone"; return; }
  rest="${stat#*) }"
  # after the comm field: state ppid pgrp session tty_nr tpgid flags minflt cminflt
  # majflt cmajflt utime stime
  echo "$(echo "$rest" | awk '{print $12" "$13}')"
}

# `/proc/<pid>/task` is the whole picture; `Threads:` in `status` is one line of it, and
# reading only that is how a lost wakeup hides: the test thread and the runtime workers are
# different threads and only one of them tells you anything.
echo "== process $PID ($(tr -d '\0' < "/proc/$PID/comm" 2>/dev/null)) =="
echo "cwd: $(readlink "/proc/$PID/cwd" 2>/dev/null)"
echo
echo "== threads =="
for t in /proc/"$PID"/task/*; do
  tid="$(basename "$t")"
  comm="$(tr -d '\0' < "$t/comm" 2>/dev/null)"
  # `/proc/<pid>/task/<tid>/stat` field 1 is the pid and field 2 is the comm **in parentheses**,
  # which itself contains spaces — so awk's split shifts every field after it. The state is
  # therefore not `$3`, and utime/stime are not `$14`/`$15`. Cut the comm out first; the first
  # token after the final `)` is the state, and utime/stime are the 12th and 13th after it.
  read -r state utime stime <<<"$(sed 's/.*) //' "$t/stat" 2>/dev/null | awk '{print $1, $12, $13}')"
  wchan="$(cat "$t/wchan" 2>/dev/null)"
  printf '  tid=%s comm=%-22s state=%s utime=%s stime=%s wchan=%s\n' "$tid" "$comm" "$state" "$utime" "$stime" "$wchan"
done
echo

A="$(cpu_of "$PID")"
echo "== sampling ${WINDOW}s (the CPU delta is the finding; a single %CPU sample is not) =="
sleep "$WINDOW"
B="$(cpu_of "$PID")"

if [ "$A" = "gone" ] || [ "$B" = "gone" ]; then
  echo "  the process exited during the window — read the test log, it is not hung."
elif [ -z "$B" ]; then
  echo "  CPU delta: (unreadable)"
else
  DELTA="$(awk -v a="$A" -v b="$B" '{print (b1-a1), (b2-a2)}' </dev/null 2>/dev/null; \
          echo "$A $B" | awk '{print ($3-$1), ($4-$2)}')"
  echo "  CPU delta (utime stime): $DELTA"
  echo "$DELTA" | grep -q '^0 0' \
    && echo "  -> ZERO: not working. Waiting on a future or a lock. Not slow I/O." \
    || echo "  -> moving: genuinely slow or genuinely working; check load before blaming it."
fi
echo

echo "== postgres :$PG_PORT/$PG_DB =="
if command -v psql >/dev/null 2>&1; then
  PGPASSWORD="${QA_PG_PASSWORD:-omnion}" psql -h 127.0.0.1 -p "$PG_PORT" -U omnion -d "$PG_DB" -tAc \
    "select coalesce(string_agg(pid||' '||state||' '||coalesce(wait_event_type,'-')||' '||coalesce(wait_event,'-')||' | '||left(query,70), E'\n'), '') from pg_stat_activity where datname='$PG_DB'" 2>&1 \
    | sed 's/^/  /'
  echo "  -- non-idle only (empty above the ClientRead rows means: the database is NOT the reason) --"
  PGPASSWORD="${QA_PG_PASSWORD:-omnion}" psql -h 127.0.0.1 -p "$PG_PORT" -U omnion -d "$PG_DB" -tAc \
    "select count(*) from pg_stat_activity where datname='$PG_DB' and state <> 'idle'" 2>&1 | sed 's/^/  active queries: /'
  PGPASSWORD="${QA_PG_PASSWORD:-omnion}" psql -h 127.0.0.1 -p "$PG_PORT" -U omnion -d "$PG_DB" -tAc \
    "select coalesce(string_agg(locktype||' '||mode||' granted='||granted, ', '), '') from pg_locks where locktype in ('advisory','transactionid')" 2>&1 | sed 's/^/  locks: /'
  PGPASSWORD="${QA_PG_PASSWORD:-omnion}" psql -h 127.0.0.1 -p "$PG_PORT" -U omnion -d postgres -tAc \
    "select 'backends='||count(*) from pg_stat_activity" 2>&1 | sed 's/^/  /'
else
  echo "  psql is not installed; the database half of this answer stays open."
fi
echo

echo "== redis :$REDIS_PORT =="
if command -v redis-cli >/dev/null 2>&1; then
  BLOCKED="$(redis-cli -h 127.0.0.1 -p "$REDIS_PORT" info clients 2>/dev/null | awk -F: '/^blocked_clients/{print $2}' | tr -d '\r')"
  CLIENTS="$(redis-cli -h 127.0.0.1 -p "$REDIS_PORT" info clients 2>/dev/null | awk -F: '/^connected_clients/{print $2}' | tr -d '\r')"
  REJ="$(redis-cli -h 127.0.0.1 -p "$REDIS_PORT" info stats 2>/dev/null | awk -F: '/^rejected_connections/{print $2}' | tr -d '\r')"
  echo "  connected=$CLIENTS blocked=${BLOCKED:-?} rejected=$REJ"
  [ "${BLOCKED:-0}" = "0" ] && echo "  -> blocked_clients is 0: Redis is NOT the reason." \
                             || echo "  -> blocked_clients > 0: something is parked on Redis."
else
  echo "  redis-cli is not installed; the Redis half stays open."
fi
echo
echo "Reading this table: CPU 0 + no non-idle query + Redis not blocked == the wait is inside"
echo "the process. Suspect the fixture's shared state (a global OnceLock/Mutex installed by an"
echo "earlier test, a spawn_blocking on a runtime with no blocking pool) — not the environment."
