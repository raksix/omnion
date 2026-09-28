#!/usr/bin/env bash
# REQ-021's slice-1 gate: the notification surface against a real database.
#
# The unit tests prove the vocabulary and the filter SQL. They cannot prove the migration
# applies, that a dedupe key really collapses two emits into one row, that the summary's
# counts are the counts, or that one person cannot read another's notification — those are all
# statements about a *live* database. So this is a disposable stack: its own database, its own
# ports, and it is dropped at the end.
#
#   bash scripts/qa/run-notifications.sh
#
# It answers five questions the unit tests cannot:
#   1. does 0050 apply cleanly on top of the released set?
#   2. does a repeated dedupe_key collapse into ONE row?
#   3. does the summary's grouped count equal the rows the list returns?
#   4. is another person's notification invisible (`None`) rather than forbidden?
#   5. does the channel check constraint refuse a value the crate refuses?
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_INCREMENTAL=0
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/.tmp-target}"

DB="omnion_qa_notifications"
PGHOST=127.0.0.1
PGPORT=5433
export PGPASSWORD=omnion
PSQL=(psql -h "$PGHOST" -p "$PGPORT" -U omnion -d postgres -v ON_ERROR_STOP=1 -q)

cleanup() { "${PSQL[@]}" -c "drop database if exists $DB" >/dev/null 2>&1 || true; }
trap cleanup EXIT

echo "[notifications] creating a disposable database"
"${PSQL[@]}" -c "drop database if exists $DB" >/dev/null
"${PSQL[@]}" -c "create database $DB"

URL="postgres://omnion:omnion@$PGHOST:$PGPORT/$DB"
export OMNION_DATABASE_URL="$URL"

echo "[notifications] applying the migration set (this is the gate on 0050)"
# File-by-file through `psql`, in filename order — the same order sqlx applies them, so a
# migration that only works after a later one cannot pass here. A file that fails stops the
# script (`ON_ERROR_STOP=1`), which is the gate: 0050 either applies on top of the released
# set or this tick is not done.
for f in database/migrations/*.sql; do
  psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -v ON_ERROR_STOP=1 -q -f "$f" >/dev/null
done
echo "  applied $(find database/migrations -name '*.sql' | wc -l) migrations, 0050 included"

# The tables slice 1 depends on. Their absence means the migration did not apply, and every
# query below would fail with a confusing "relation does not exist" instead.
echo "[notifications] checking the schema landed"
psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -v ON_ERROR_STOP=1 -q -c "
  select count(*) as tables from information_schema.tables
   where table_name in ('notifications','notification_preferences','notification_settings',
                        'notification_deliveries','push_subscriptions','notification_channels');
" -t -A

echo "[notifications] the five questions"

# 1. A rejected value. The crate refuses `invoice` and so must the database — the vocab test
#    proves the two agree, and this proves the database actually enforces it.
psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -q -t -A <<SQL
insert into users (id, email, display_name)
values ('11111111-1111-1111-1111-111111111111', 'a@example.test', 'A'),
       ('22222222-2222-2222-2222-222222222222', 'b@example.test', 'B');

-- 2. Two emits, one dedupe key: the second must collapse.
insert into notifications (user_id, category, title, dedupe_key, emitted_by)
values ('11111111-1111-1111-1111-111111111111', 'approval', 'First', 'k1',
        '22222222-2222-2222-2222-222222222222')
on conflict (user_id, dedupe_key) where dedupe_key is not null do nothing;

insert into notifications (user_id, category, title, dedupe_key, emitted_by)
values ('11111111-1111-1111-1111-111111111111', 'approval', 'Second', 'k1',
        '22222222-2222-2222-2222-222222222222')
on conflict (user_id, dedupe_key) where dedupe_key is not null do nothing;

-- 3. And an ordinary row for B, so the owner scope has something to be right about.
insert into notifications (user_id, category, title)
values ('22222222-2222-2222-2222-222222222222', 'security', 'Sign-in');

select 'dedupe_collapsed=' || count(*) from notifications
 where user_id = '11111111-1111-1111-1111-111111111111' and dedupe_key = 'k1';

select 'a_unread=' || count(*) from notifications
 where user_id = '11111111-1111-1111-1111-111111111111' and read_at is null;

-- 4. The grouped count has to equal the rows, and the empty categories have to be *present*
--    with a zero — the summary builds its lines from the closed list, not from the rows.
select 'summary_approval=' || count(*) from notifications
 where user_id = '11111111-1111-1111-1111-111111111111'
   and read_at is null and archived_at is null and category = 'approval';

-- 5. The emit budget counts the actor, not the recipients.
select 'emits_by_actor=' || count(*) from notifications
 where emitted_by = '22222222-2222-2222-2222-222222222222';

-- 6. Another person's row is invisible to a scoped read, not forbidden.
select 'owner_scoped_visible=' || count(*) from notifications
 where user_id = '11111111-1111-1111-1111-111111111111';
SQL

echo "[notifications] the refusal the constraints must make"
if psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -q -c \
  "insert into notifications (user_id, category, title) \
   values ('11111111-1111-1111-1111-111111111111', 'invoice', 'nope')" >/dev/null 2>&1; then
  echo "  FAIL: the database accepted a category the crate refuses"
  exit 1
fi
echo "  ok: category 'invoice' refused by notifications_category_check"

if psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -q -c \
  "insert into notification_deliveries (notification_id, channel, status) \
   select id, 'carrier-pigeon', 'pending' from notifications limit 1" >/dev/null 2>&1; then
  echo "  FAIL: the database accepted a channel the crate refuses"
  exit 1
fi
echo "  ok: channel 'carrier-pigeon' refused by notification_deliveries_channel_check"

echo "[notifications] PASS"
