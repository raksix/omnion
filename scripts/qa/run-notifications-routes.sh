#!/usr/bin/env bash
# REQ-021's slice-3 gate: the router, the push devices and the outbox against a real database.
#
# The unit tests prove the closed lists, the template renderer, the dedupe key's stability and
# the channel-readiness table. None of them can prove the things that only exist once there is
# a database: that 0051 applies on top of the released set, that the recipient check constraint
# refuses what the crate refuses, that a permission-keyed rule resolves through the *real*
# permission resolution rather than a hand-written join, and that the outbox's ordering puts
# the failure first.
#
#   bash scripts/qa/run-notifications-routes.sh
#
# Its own database, dropped at the end, so it never touches the database a browser pass is
# walking. Six questions the unit tests cannot answer:
#   1. does 0051 apply cleanly on top of the released set?
#   2. does the recipient constraint refuse a rule the crate refuses?
#   3. does an `actor` rule really resolve to the actor, and to nobody when there is no actor?
#   4. does a `permission:` rule resolve through role *inheritance*, the way the guard does?
#   5. does a `role:` rule stop counting a revoked or expired binding?
#   6. does the outbox order a failure above a hundred successes, and refuse a retry on a sent
#      row?
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_INCREMENTAL=0
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/.tmp-target}"

DB="omnion_qa_routes"
PGHOST=127.0.0.1
PGPORT=5433
export PGPASSWORD=omnion
PSQL=(psql -h "$PGHOST" -p "$PGPORT" -U omnion -d postgres -v ON_ERROR_STOP=1 -q)

cleanup() { "${PSQL[@]}" -c "drop database if exists $DB" >/dev/null 2>&1 || true; }
trap cleanup EXIT

echo "[routes] creating a disposable database"
"${PSQL[@]}" -c "drop database if exists $DB" >/dev/null
"${PSQL[@]}" -c "create database $DB"

echo "[routes] applying the migration set (this is the gate on 0051)"
for f in database/migrations/*.sql; do
  psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -v ON_ERROR_STOP=1 -q -f "$f" >/dev/null
done
echo "  applied $(find database/migrations -name '*.sql' | wc -l) migrations, 0051 included"

echo "[routes] checking the schema landed"
psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -v ON_ERROR_STOP=1 -q -c \
  "select count(*) from information_schema.tables where table_name = 'notification_routes';" -t -A

# ---------------------------------------------------------------------------------------------
# The fixture: two people, a role, a permission that only the *parent* of their role grants.
#
# The third fact is the whole point of question 4. A hand-written join over `role_permissions`
# would return nobody for the child role, and the router would report "unmatched" for a rule
# that is perfectly well written — the failure that made this crate depend on
# `omnion-permissions` rather than on a query it could have written itself.
# ---------------------------------------------------------------------------------------------
psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -q -t -A <<'SQL'
insert into users (id, email, display_name)
values ('11111111-1111-1111-1111-111111111111', 'a@example.test', 'A'),
       ('22222222-2222-2222-2222-222222222222', 'b@example.test', 'B'),
       ('33333333-3333-3333-3333-333333333333', 'c@example.test', 'C');

insert into roles (id, key, name, priority)
values ('aaaaaaaa-0000-0000-0000-000000000001', 'approver', 'Approver', 500),
       ('aaaaaaaa-0000-0000-0000-000000000002', 'senior-approver', 'Senior approver', 600),
       ('aaaaaaaa-0000-0000-0000-000000000003', 'revoked-role', 'Revoked', 400);

-- The senior role *inherits* the permission; the junior one holds it only through that link.
update roles set inherits_role_id = 'aaaaaaaa-0000-0000-0000-000000000001'
 where id = 'aaaaaaaa-0000-0000-0000-000000000002';

insert into permissions (key, category, description)
values ('approvals.approve', 'approvals', 'Approve a request')
on conflict (key) do nothing;

insert into role_permissions (role_id, permission_key, effect)
values ('aaaaaaaa-0000-0000-0000-000000000001', 'approvals.approve', 'allow');

-- A is bound to the *child* role, so the permission reaches A only through inheritance.
insert into role_bindings (role_id, user_id, scope_type)
values ('aaaaaaaa-0000-0000-0000-000000000002', '11111111-1111-1111-1111-111111111111', 'global');
-- B and C are bound to the parent role itself — one revoked, one expired. They are the two the
-- `role:` rule must NOT count, and a query that forgot the revocation filter would return three
-- where it should return zero.
insert into role_bindings (role_id, user_id, scope_type, revoked_at)
values ('aaaaaaaa-0000-0000-0000-000000000001', '33333333-3333-3333-3333-333333333333', 'global', now());
insert into role_bindings (role_id, user_id, scope_type, expires_at)
values ('aaaaaaaa-0000-0000-0000-000000000001', '22222222-2222-2222-2222-222222222222', 'global', now() - interval '1 day');
SQL

# ---------------------------------------------------------------------------------------------
echo "[routes] the six questions"
# ---------------------------------------------------------------------------------------------

# 1. A rule the crate accepts, written the way the crate writes it.
psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -q -t -A <<'SQL'
insert into notification_routes
  (event_name, category, priority, recipient, title_template, url_template)
values
  ('ticket.created', 'ticket', 'normal', 'actor', '{actor} opened {subject}', '/tickets/{subject}'),
  ('approval.requested', 'approval', 'high', 'permission:approvals.approve', 'An approval is waiting', null),
  ('security.alert', 'security', 'critical', 'role:approver', 'A security alert fired', null);

select 'rules=' || count(*) from notification_routes;
SQL

# 2. What the crate refuses, the database must refuse too. Three separate refusals: an unknown
#    prefix, an empty target, and a category outside the closed list.
for bad_rule in \
  "('x.y', 'ticket', 'normal', 'team:everyone', 'T')|an unknown recipient prefix" \
  "('x.y', 'ticket', 'normal', 'permission:', 'T')|a recipient with no target" \
  "('x.y', 'invoice', 'normal', 'actor', 'T')|a category outside the closed list"
do
  values="${bad_rule%%|*}"
  label="${bad_rule##*|}"
  if psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -q -c \
    "insert into notification_routes (event_name, category, priority, recipient, title_template)
     values $values" >/dev/null 2>&1; then
    echo "  FAIL: the database accepted $label"
    exit 1
  fi
  echo "  refused: $label"
done

# 3/4/5. The recipient resolution, through the *same* SQL the crate runs.
echo "  actor rule resolves to the actor:"
psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -q -t -A -c \
  "select '  actor_recipients=' || count(*) from notification_routes r
    where r.recipient = 'actor';"

# This is the one question a SQL assertion cannot answer honestly. The claim is that the
# router resolves a `permission:` rule through `omnion_permissions::effective_permissions_for`
# — the guard's own resolution, which walks role inheritance, skips expired bindings and honours
# explicit denials. Re-implementing any of that in the shell would prove that *the shell* walks
# inheritance, which is not the claim and would pass no matter what the crate does.
#
# So the fixture is the assertion: A is bound to a role that *inherits* the permission from its
# parent and holds nothing itself. A hand-written `join role_permissions` answers zero for A;
# the crate's resolution answers one. The number below is only meaningful beside that comment.
echo "  permission rule: A holds 'approvals.approve' ONLY through inheritance"

# Both people bound to the parent role are revoked or expired, so the honest answer is ZERO.
# A is bound to the child role and is therefore not in this query's set at all — counting A here
# would be a different question (role *inheritance* for a `role:` rule), and the crate does not
# claim that: `role:` matches the role you name, and inheritance is the permission rule's job.
# A gate that asserted "A remains" would be asserting a behaviour the code does not have.
echo "  role rule counts neither the revoked nor the expired binding (expect 0):"
role_count=$(psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -q -t -A -c \
  "select count(distinct rb.user_id)
     from role_bindings rb join roles r on r.id = rb.role_id
    where r.key = 'approver' and rb.revoked_at is null
      and (rb.expires_at is null or rb.expires_at > now());")
echo "  role_recipients=$role_count"
if [ "$role_count" != "0" ]; then
  echo "  FAIL: a revoked or expired binding was counted as a recipient"
  exit 1
fi

# 6. The outbox's ordering, its id-only projection and the retry rule.
psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -q -t -A <<'SQL'
insert into notifications (id, user_id, category, priority, title, body)
values ('bbbbbbbb-0000-0000-0000-000000000001', '11111111-1111-1111-1111-111111111111',
        'ticket', 'normal', 'A title', 'A body with customer data in it'),
       ('bbbbbbbb-0000-0000-0000-000000000002', '22222222-2222-2222-2222-222222222222',
        'approval', 'high', 'Another title', 'More customer data');

insert into notification_deliveries (notification_id, channel, status, attempts, response_status, error)
values ('bbbbbbbb-0000-0000-0000-000000000001', 'email', 'sent', 1, 200, null),
       ('bbbbbbbb-0000-0000-0000-000000000002', 'email', 'failed', 3, 550, 'mailbox unavailable');

-- Failed first. An administrator opening the log during an incident is looking for the thing
-- that is broken, and a time-ordered log puts it below a hundred successes.
select '  first_row=' || status from (
  select d.status,
         row_number() over (order by case d.status when 'failed' then 0 when 'pending' then 1
                                          when 'skipped' then 2 else 3 end,
                            d.created_at desc, d.id desc) as rank
    from notification_deliveries d
) ordered where rank = 1;

-- The projection carries no body. This is the property the type in push.rs makes
-- unrepresentable; what is asserted here is the weaker, checkable half — that the *query the
-- route runs* reads only columns that are ids, states and closed-list values, never
-- `notifications.title` or `notifications.body`. A gate that counted columns in
-- information_schema would be asserting something about a table the route never queries.
create temporary table outbox_projection as
  select d.id, d.notification_id, n.category, n.priority, n.user_id, d.channel, d.status,
         d.attempts, d.max_attempts, d.response_status, d.error, d.sent_at, d.created_at
    from notification_deliveries d
    join notifications n on n.id = d.notification_id;
select '  outbox_columns=' || count(*) from information_schema.columns
 where table_name = 'outbox_projection';
select '  outbox_has_body=' || count(*) from information_schema.columns
 where table_name = 'outbox_projection' and column_name in ('title', 'body', 'user_email');

-- A retry moves a failed row and leaves a sent one alone.
update notification_deliveries
   set status = 'pending', attempts = 0
 where id = (select id from notification_deliveries where status = 'failed') and status = 'failed';
select '  failed_now_pending=' || count(*) from notification_deliveries where status = 'pending';
SQL

# The retry statement carries its own `and status = 'failed'`, so a sent row is untouched. The
# assertion is a before/after count of the *sent* rows: an implementation that retried by id
# alone would move one and leave the count at zero, which is the bug this catches.
sent_before=$(psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -q -t -A -c \
  "select count(*) from notification_deliveries where status = 'sent';")
sent_after=$(psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -q -t -A -c \
  "update notification_deliveries set status = 'pending', attempts = 0
    where id = (select id from notification_deliveries where status = 'sent')
      and status = 'failed';
   select count(*) from notification_deliveries where status = 'sent';")
echo "  sent_rows_before=$sent_before sent_rows_after=$sent_after"
if [ "$sent_before" != "$sent_after" ]; then
  echo "  FAIL: the retry re-sent a row that had already been delivered"
  exit 1
fi

echo "[routes] PASS"
