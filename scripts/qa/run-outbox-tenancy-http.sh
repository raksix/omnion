#!/usr/bin/env bash
# The tenancy boundary of the ADMIN DELIVERY LOG, over a real socket with two real tenants.
#
#   QA_DB=omnion_qa_w8_outbox_tenant bash scripts/qa/run-outbox-tenancy-http.sh
#
# ## Why this gate exists
#
# Tick 75 fixed the WRITE side of the router: every row now carries the RECIPIENT's tenant,
# and `addressable_recipients` returns `(id, organization)` pairs so a caller cannot bind the
# sender's column by mistake. The tick closed on the sentence its own fix had just finished
# saying:
#
#   *What has never been asked: the admin outbox takes `session.user.organization_id` as a
#   QUERY ARGUMENT.*
#
# It had. Two read paths take the caller's organization as a parameter rather than deriving
# it, and each one decides what "no organization" means differently:
#
#   * `push::list_outbox` — `else { n.organization_id is null }`. A session whose
#     `users.organization_id` is null reads PLATFORM traffic.
#   * `push::outbox_counts` — `($1::uuid is null and n.organization_id is null) or
#     n.organization_id = $1::uuid`. Same policy, re-spelled in a second place, which is how
#     the two halves of one screen drifted apart in the first place.
#
# So a tenant administrator holding `notifications.admin` whose account was never attached to
# an organization sees a delivery log full of rows belonging to no tenant — and, worse, a log
# that omits every row belonging to THEIR OWN tenant, because those rows are stamped with a
# tenant id and the session has none to match.
#
# ## What it drives
#
# Eight legs, in this order, and the order is load-bearing:
#
#   0. the premise (B's session resolves to tenant B, and B reaches the handler at all) —
#      `403 considered: 0` says "no rule even looked", the tick 72 failure;
#   1. a **positive control**: B reads its own tenant's log and sees its own row. A gate whose
#      every assertion is "no stranger's row" is also satisfied by a route that returns
#      nothing to anybody — the shape `run-crm-intake` hit at tick 68;
#   2. the neighbour of that control — neither tenant's log lists the other tenant's traffic;
#   3. the orgless arm: an account with no organization is the platform operator and is
#      answered with the PLATFORM audience, which is the rows stamped to no tenant;
#   4. the counts agree with the rows — the screen shows chips, so a route that filtered the
#      list but not the counts would render "3" above an empty log;
#   5. **the retry button is scoped too**: a tenant administrator cannot requeue a delivery
#      belonging to another tenant by id. `retry_delivery` took only a uuid before this slice,
#      so the row was addressable by anyone who could guess or learn one — and a retry is a
#      WRITE that hands the message back to the transport;
#   6. **a tenant still reads the platform's announcement** (`or n.organization_id is null`).
#      Without this clause an administrator reads an EMPTY delivery log during an outage, and
#      the gate cannot tell "correctly scoped" from "scoped to nothing";
#   7. the platform account sees platform traffic — the arm that explains why the fix is not
#      "every orgless session sees everything".
#
# **All three delivery rows are written by the FIXTURE.** The platform row was originally
# inserted in the last leg, so no earlier leg could observe it: leg 6 could not have failed,
# and the neutralised control came back fully green with the tenant clause missing. A row
# inserted after a measurement cannot change that measurement.
#
# ## Its own database, never the pass's
#
# The model is `run-notification-tenancy-http.sh`, and so is the refusal to start when
# pointed at a database whose name is not its own: this gate writes notifications, deliveries
# and sessions, and a run that shares `omnion_qa_w8` with a browser pass would leave rows
# the pass then counts.
#
# ## The port is not assumed
#
# Nine sibling loops reach their APIs over loopback and two of them are currently holding
# `:18086` and `:18085` with their own worktree as cwd. An orphan API answers `/readyz` 200
# for a process that migrated a DIFFERENT database, so the gate's first failure then names
# `relation "notifications" does not exist` and points at the wrong subsystem. The gate
# takes `QA_API_PORT` and refuses to continue until the process answering `/readyz` is the
# one this run started.
#
# ## Running it
#
#   bash scripts/qa/run-outbox-tenancy-http.sh            # its own database, its own port
#   QA_API_PORT=18089 bash scripts/qa/run-outbox-tenancy-http.sh
#
# PROVEN TO FAIL: re-run with `QA_NEUTRALISE=outbox-scope`, which puts the pre-fix policy back:
# `push_outbox_scope` returns `list_outbox`'s original two-way split, and `retry_delivery`
# loses its predicate entirely (it had none). Every tenancy leg must go red and every
# tenancy-free neighbour must stay green — if the whole file goes red, the control removed
# the route rather than the rule.

set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.." || exit 1

QA_DB="${QA_DB:-omnion_qa_w8_outbox_tenant}"
QA_API_PORT="${QA_API_PORT:-18088}"
QA_CARGO_TARGET_DIR="${QA_CARGO_TARGET_DIR:-/dev/shm/w8-target}"
API_BIN="${QA_CARGO_TARGET_DIR}/debug/omnion-api"
BASE="http://127.0.0.1:${QA_API_PORT}"
# The port is the box's, not the default: PostgreSQL for the w8 stacks answers on 5433, and
# a wrong port is a connection refusal that the readiness check reports as "the api did
# not become ready" while the real cause is this line.
export OMNION_DATABASE_URL="postgres://omnion:omnion@127.0.0.1:${QA_PG_PORT:-5433}/${QA_DB}"

# **The template needs the X's.** `mktemp -t outbox-tenancy` exits 1 with "too few X's", the
# command substitution swallows the message, and `LOG` is empty — so the api's output line
# becomes `>>""`, the redirect fails, and the readiness check then reports "the api did not
# become ready" while the real cause is a file name four lines above. A failed command
# substitution is the kind of failure that reads like the thing it prevented.
LOG="$(mktemp -t outbox-tenancy.XXXXXX)"
API_PID=""
LEG=0
TOTAL=8

# --- counting ---------------------------------------------------------------------------------
# `leg` is the only counter. A verb used for two purposes cannot be audited, which is how a
# previous gate printed `PASS 16/7`.
leg() {
	LEG=$((LEG + 1))
	printf '  leg %d/%d %s\n' "$LEG" "$TOTAL" "$1"
}
ok()   { printf '     ok   %s\n' "$1"; }
bad()  { printf '     FAIL %s\n' "$1"; FAILED=1; }
FAILED=0

# The summary refuses to print unless every leg ran. Seven green lines out of seven expected
# is a pass; six out of seven is a leg that never executed, and printing a summary there is the
# silent-gate shape this branch has met twice. `TOTAL` is a hand-written number, so adding a
# leg without bumping it makes the gate print a PASS one leg short — the counting has to be
# checked against the file, not trusted.
summary() {
	if [ "$LEG" -ne "$TOTAL" ]; then
		printf '\nINCOMPLETE %d/%d legs ran — refusing to print a verdict\n' "$LEG" "$TOTAL"
		exit 1
	fi
	if [ "$FAILED" -ne 0 ]; then
		# **Two counters, two numbers, and this is the second time this branch has had to
		# write it down.** The first draft printed `FAIL %d/%d legs failed` with `$LEG` and
		# `$TOTAL` in it, which is the legs *that ran* under a label saying *failed*: a run
		# with two red legs printed `FAIL 8/8 legs failed`, which reads as "the control took
		# the whole route out" — the exact conclusion this gate's header tells the reader to
		# watch for, asserted by the gate itself. A green/red verdict whose wording
		# contradicts the per-leg lines above it costs more than no summary at all.
		printf '\nFAIL — %d of %d legs ran, %d failed\n' "$LEG" "$TOTAL" "$FAILED"
		exit 1
	fi
	printf '\nPASS %d/%d\n' "$LEG" "$TOTAL"
	exit 0
}

cleanup() {
	if [ -n "$API_PID" ] && kill -0 "$API_PID" 2>/dev/null; then
		kill -9 "$API_PID" 2>/dev/null
		wait "$API_PID" 2>/dev/null
	fi
}
trap cleanup EXIT

# --- preconditions ---------------------------------------------------------------------------
case "$QA_DB" in
	*w8*) ;;
	*)
		printf 'refusing to run against %s: this gate needs its own w8 database\n' "$QA_DB" >&2
		exit 1
		;;
esac

if [ "${QA_NEUTRALISE:-}" = "outbox-scope" ]; then
	printf 'NEUTRALISED: the outbox tenancy predicate is replaced with the pre-fix body\n'
fi

printf 'building omnion-api into %s\n' "$QA_CARGO_TARGET_DIR"
if [ ! -x "$API_BIN" ] || [ -n "${QA_REBUILD:-}" ]; then
	PATH="$HOME/.cargo/bin:$PATH" CARGO_TARGET_DIR="$QA_CARGO_TARGET_DIR" \
		cargo build -p omnion-api --quiet || {
		printf 'build failed\n' >&2
		exit 1
	}
fi

# A port held by somebody else is the failure this gate exists to not misread.
HOLDER=""
for p in $(pgrep -x omnion-api 2>/dev/null); do
	if [ "$p" = "$$" ]; then continue; fi
	PPORT=$(tr '\0' '\n' < "/proc/$p/environ" 2>/dev/null | grep -m1 '^OMNION_PORT=' | cut -d= -f2)
	if [ "$PPORT" = "$QA_API_PORT" ]; then
		HOLDER="$p ($(readlink "/proc/$p/cwd" 2>/dev/null))"
	fi
done
if [ -n "$HOLDER" ]; then
	printf 'port %s is held by another omnion-api: %s\n' "$QA_API_PORT" "$HOLDER" >&2
	printf 'pick another with QA_API_PORT=...\n' >&2
	exit 1
fi

# --- the limiter's own Redis index, decided before the api starts ------------------------------
# **Assigned here, before the api is started, because the first draft assigned it after.** The
# api reads `OMNION_REDIS_URL` from its environment at boot; a `REDIS_DB=15` written twenty
# lines further down changed nothing for the server and left the gate's three sign-ins in the
# box's shared db 0 — where `run-notification-tenancy-http.sh` measured 25 against a ceiling of
# 10, so this gate would have read a 401 as a passing "refused" leg. Nine sibling loops share
# this box, which is what makes the index worth its own.
REDIS_DB="${QA_OUTBOX_TENANCY_REDIS_DB:-15}"

printf 'starting the api on :%s against %s (limiter counters in redis db %s)\n' \
	"$QA_API_PORT" "$QA_DB" "$REDIS_DB"
# `OMNION_CSRF_SECRET` decides whether a cookie-authenticated MUTATION is refused before it
# reaches a handler. Without it every POST this gate makes — creating the tenants' accounts,
# pressing the retry button — answers `403 csrf_unavailable`, and the run dies on a permission
# message that names the wrong subsystem entirely. Measured here, not guessed: the first
# attempt to seed a tenant account answered exactly that, with an empty-looking body.
OMNION_CSRF_SECRET="${OMNION_CSRF_SECRET:-qa-local-throwaway-value}" \
OMNION_REDIS_URL="redis://127.0.0.1:${QA_REDIS_PORT:-6380}/${REDIS_DB}" \
QA_NEUTRALISE="${QA_NEUTRALISE:-}" \
OMNION_PORT="$QA_API_PORT" \
OMNION_ENV=development \
	"$API_BIN" >>"$LOG" 2>&1 &
API_PID=$!

READY=0
for _ in $(seq 1 60); do
	if curl -fsS "${BASE}/readyz" >/dev/null 2>&1; then READY=1; break; fi
	if ! kill -0 "$API_PID" 2>/dev/null; then break; fi
	sleep 1
done
if [ "$READY" -ne 1 ]; then
	printf 'the api did not become ready; last lines:\n' >&2
	tail -20 "$LOG" >&2
	exit 1
fi
# /readyz answers 200 for a process that migrated a DIFFERENT database, so the fixture check
# below is the one that decides whether this run is measuring its own database.
TABLES=$(psql "$OMNION_DATABASE_URL" -tAc \
	"select count(*) from information_schema.tables where table_name = 'notification_deliveries'" 2>/dev/null)
if [ "${TABLES:-0}" != "1" ]; then
	printf 'the api on :%s is not serving %s (notification_deliveries absent there)\n' \
		"$QA_API_PORT" "$QA_DB" >&2
	exit 1
fi

# --- fixture ----------------------------------------------------------------------------------
# Two tenants, three accounts that sign in, and one ORGLESS account — the population that made
# this defect reachable at all. Both tenants get a real delivery row so "the log is empty" and
# "the log is filtered" are distinguishable from every angle.
#
# The first-run flow is once-only, so the accounts are created through the real IAM route and
# the previous run's rows go first. A hand-written `users` row cannot sign in: its
# `password_hash` is not a hash of the password the gate sends, and a wrong hash and a tenancy
# refusal both answer 401 — so a fixture written by hand makes the "refused, therefore safe"
# legs pass against an installation where nobody was ever logged in.
psql "$OMNION_DATABASE_URL" -v ON_ERROR_STOP=1 -q <<'SQL'
delete from sessions where true;
delete from notification_deliveries where true;
delete from notifications where true;
-- Bindings BEFORE users: the rows are keyed on a generated user id, so a delete that names
-- last run's ids cannot reach them, and the stale global binding left behind made the
-- count check below read 4 instead of 3.
delete from role_permissions where true;
delete from role_bindings where true;
delete from organizations where slug in ('qa-outbox-a', 'qa-outbox-b');
delete from users where true;
SQL

# **`psql -tA` prints the command tag too**, so `returning id` captures `<uuid>INSERT0 1` and
# the trailing text rides into the JSON body below — where it surfaces as "UUID parsing failed:
# found `I` at 36", four steps from the psql that caused it. `-q` is what silences the tag; the
# uuid is then the whole of stdout.
org_of() {
	psql "$OMNION_DATABASE_URL" -tAq -c \
		"insert into organizations (name, slug, status, created_at, updated_at)
		 values ('$1', '$2', 'active', now(), now()) returning id" | tr -d ' \n'
}
ORG_A=$(org_of 'QA Outbox A' 'qa-outbox-a')
ORG_B=$(org_of 'QA Outbox B' 'qa-outbox-b')
if [ -z "$ORG_A" ] || [ -z "$ORG_B" ]; then
	printf 'the tenants organizations could not be created\n' >&2
	exit 1
fi
printf 'tenants %s (A) and %s (B)\n' "$ORG_A" "$ORG_B"

# The two delivery rows and the platform row, named once. Written out by hand this literal was
# one character short in all eleven copies, which PostgreSQL rejected as a uuid parse error
# while the gate's first four legs were already reporting on rows that did not exist.
DELIVERY_A="$(printf 'dddddddd-dddd-dddd-dddd-ddddddd%05d' 1)"
DELIVERY_B="$(printf 'dddddddd-dddd-dddd-dddd-ddddddd%05d' 2)"
DELIVERY_P="$(printf 'dddddddd-dddd-dddd-dddd-ddddddd%05d' 9)"

# `grep -o` of a whole uuid, never a sed character class that admits a hyphen and stops at the
# end of the first group.
uuid_of() { grep -o '[0-9a-f]\{8\}-[0-9a-f]\{4\}-[0-9a-f]\{4\}-[0-9a-f]\{4\}-[0-9a-f]\{12\}' | head -1; }

# The platform account: `organization_id is null`, which is the whole population leg 3 and
# leg 6 are about. It is the owner created by the once-only first-run flow.
rm -f /tmp/outbox-tenancy-root.jar
OWNER=$(curl -sS -c /tmp/outbox-tenancy-root.jar -X POST "${BASE}/api/v1/onboarding/owner" \
	-H 'content-type: application/json' \
	-d '{"display_name":"QA Platform","email":"outbox-root@qa.test","password":"qa-password-123"}')
ROOT_USER=$(printf '%s' "$OWNER" | uuid_of)
if [ -z "$ROOT_USER" ]; then
	printf 'the platform owner could not be created through the real route: %s\n' \
		"$(printf '%s' "$OWNER" | head -c 200)" >&2
	exit 1
fi
printf 'platform owner %s (no organization)\n' "$ROOT_USER"

# One account inside each tenant, with a real password and a real organization attach.
mk_tenant_user() {
	curl -sS -X POST "${BASE}/api/v1/iam/users" -b /tmp/outbox-tenancy-root.jar \
		-H 'content-type: application/json' \
		-d "{\"email\":\"$1\",\"display_name\":\"$2\",\"password\":\"qa-password-123\",\"organization_id\":\"$3\"}"
}
A_BODY=$(mk_tenant_user 'outbox-a@qa.test' 'QA Outbox A' "$ORG_A")
B_BODY=$(mk_tenant_user 'outbox-b@qa.test' 'QA Outbox B' "$ORG_B")
USER_A=$(printf '%s' "$A_BODY" | uuid_of)
USER_B=$(printf '%s' "$B_BODY" | uuid_of)
if [ -z "$USER_A" ] || [ -z "$USER_B" ]; then
	# The response body is printed, because "could not be created" without it sends the next
	# reader to the wrong file: this is either a permission refusal (the role holding
	# `iam.manage` is not the one just created) or a 422 naming a validation rule, and the two
	# have nothing in common but the missing id.
	printf 'the tenants accounts could not be created through the IAM route:\n  A: [%s]\n  B: [%s]\n' \
		"$(printf '%s' "$A_BODY" | head -c 300)" "$(printf '%s' "$B_BODY" | head -c 300)" >&2

	exit 1
fi
printf 'tenant accounts %s (A) and %s (B)\n' "$USER_A" "$USER_B"

# The permission. An orgless account can only hold it through a GLOBAL binding and a tenant
# account through an ORGANIZATION one (`role_bindings_scope_shape_check`), so the fixture
# writes the shape the constraint allows rather than the shape the test imagines — the second
# run of this gate failed on the constraint, not on the behaviour it meant to measure.
psql "$OMNION_DATABASE_URL" -v ON_ERROR_STOP=1 -q <<SQL
-- The priority column is NOT NULL on roles. Omitting it aborts the WHOLE remaining block,
-- so the bindings and the delivery rows are never written and every tenant session answers
-- 403 — which two of the legs below read as "no leak found" on an empty body. A fixture that
-- dies halfway is a fixture whose passes mean nothing.
insert into roles (id, key, name, priority, organization_id)
values ('99999999-9999-9999-9999-999999999990', 'qa-notifications-admin', 'QA Notifications Admin', 100, null)
on conflict (id) do nothing;

-- A binding is not a grant. role_bindings says WHO holds the role; role_permissions says WHAT
-- it may do, and the guard's answer without these rows is considered=1 together with
-- permission_denied: one binding considered, nothing allowed. The first draft of this fixture
-- wrote the binding alone and every tenant leg answered 403, which reads exactly like a
-- tenancy bug. The key is read off guards::require in routes/mod.rs, never off the prose — a
-- plausible-looking key that is not in the catalogue is a list of wishes.
-- (No backticks in this comment on purpose: an interpolating heredoc EXECUTES them, and three
-- of them printed "command not found" into the run's own output.)
insert into role_permissions (role_id, permission_key, effect, created_at)
values ('99999999-9999-9999-9999-999999999990', 'notifications.admin', 'allow', now())
on conflict (role_id, permission_key) do nothing;
insert into role_bindings (id, role_id, user_id, scope_type, organization_id)
values
 ('99999999-9999-9999-9999-9999999999a1', '99999999-9999-9999-9999-999999999990', '$USER_A', 'organization', '$ORG_A'),
 ('99999999-9999-9999-9999-9999999999a2', '99999999-9999-9999-9999-999999999990', '$USER_B', 'organization', '$ORG_B'),
 ('99999999-9999-9999-9999-9999999999a3', '99999999-9999-9999-9999-999999999990', '$ROOT_USER', 'global', null)
on conflict (id) do nothing;

-- One failed delivery per tenant, so the positive control has something to find and the
-- cross-tenant retry has something real to refuse.
insert into notifications (id, user_id, category, priority, title, body, organization_id, created_at)
values ('cccccccc-cccc-cccc-cccc-ccccccccccc1', '$USER_A',
        'system', 'normal', 'A row', 'for tenant A', '$ORG_A', now()),
       ('cccccccc-cccc-cccc-cccc-ccccccccccc2', '$USER_B',
        'system', 'normal', 'B row', 'for tenant B', '$ORG_B', now())
on conflict (id) do nothing;
insert into notification_deliveries (id, notification_id, channel, status, attempts, max_attempts, created_at)
values ('$DELIVERY_A', 'cccccccc-cccc-cccc-cccc-ccccccccccc1', 'email', 'failed', 3, 3, now()),
       ('$DELIVERY_B', 'cccccccc-cccc-cccc-cccc-ccccccccccc2', 'email', 'failed', 3, 3, now())
on conflict (id) do nothing;
SQL
# **The PLATFORM row is written in the FIXTURE, not injected in the last leg.** The first draft
# created it at the end and the leg before it then measured a count that could not have
# included it — so the leg that exists to catch a tenant's chips disagreeing with its list was
# blind to the one row that separates them, and the neutralised control came back 7/7 green.
# A row inserted after the measurement cannot change the measurement; that is the whole reason
# it has to be part of the fixture.
psql "$OMNION_DATABASE_URL" -v ON_ERROR_STOP=1 -q <<SQL
insert into notifications (id, user_id, category, priority, title, body, organization_id, created_at)
values ('cccccccc-cccc-cccc-cccc-ccccccccccc9', '$ROOT_USER',
        'system', 'normal', 'Platform row', 'belongs to no tenant', null, now())
on conflict (id) do nothing;
insert into notification_deliveries (id, notification_id, channel, status, attempts, max_attempts, created_at)
values ('$DELIVERY_P', 'cccccccc-cccc-cccc-cccc-ccccccccccc9', 'email', 'failed', 3, 3, now())
on conflict (id) do nothing;
SQL
# **The fixture is VERIFIED before any leg runs.** A gate whose fixture died halfway reports
# its first legs green against an empty table — "no stranger's row" is trivially true when
# there are no rows at all. Three counts, and the run stops here rather than printing a
# verdict built on a fixture that never landed.
# Count THIS gate's role, not every binding: the onboarding route gives the platform owner a
# default role of its own, so a bare `select count(*) from role_bindings` reads 4 and the first
# draft of this check called a correct fixture broken. The count that matters is the one the
# legs depend on.
BINDINGS=$(psql "$OMNION_DATABASE_URL" -tAq -c \
	"select count(*) from role_bindings where role_id = '99999999-9999-9999-9999-999999999990'" 2>/dev/null)
DELIVERIES=$(psql "$OMNION_DATABASE_URL" -tAq -c "select count(*) from notification_deliveries" 2>/dev/null)
GRANTS=$(psql "$OMNION_DATABASE_URL" -tAq -c \
	"select count(*) from role_permissions where role_id = '99999999-9999-9999-9999-999999999990'" 2>/dev/null)
# The grant is counted too: without it the guard answers `considered:1, permission_denied` on
# every route, and every tenant leg 403s while the two "no leak" legs pass on an empty body.
if [ "$BINDINGS" != "3" ] || [ "$DELIVERIES" != "3" ] || [ "$GRANTS" != "1" ]; then
	printf 'the fixture did not land: %s bindings (want 3), %s delivery rows (want 3), %s permission rows (want 1)\n' \
		"$BINDINGS" "$DELIVERIES" "$GRANTS" >&2
	exit 1
fi
printf 'fixture ready (3 bindings, 3 deliveries — two tenants and one platform)\n'

# --- sessions ---------------------------------------------------------------------------------
# The password the accounts really have, and the login body is READ, never discarded: a failure
# has to arrive here, where the email is still in scope, rather than one leg later as "session
# does not answer", which names the symptom three steps from its cause.
sign_in_for() {
	local who="$1" email="$2" want="$3" body me
	body=$(curl -sS -c "/tmp/outbox-tenancy-$who.jar" -X POST "${BASE}/api/v1/auth/login" \
		-H 'content-type: application/json' \
		-d "{\"email\":\"$email\",\"password\":\"qa-password-123\"}")
	if ! printf '%s' "$body" | grep -q 'session\|user\|token'; then
		printf 'the %s sign-in was refused: %s\n' "$email" \
			"$(printf '%s' "$body" | head -c 200)" >&2
		exit 1
	fi
	# Checked against the account it MUST be: "a cookie jar exists" and "this is the right
	# tenant's session" are different facts, and only the second one is the fixture.
	me=$(curl -sS -b "/tmp/outbox-tenancy-$who.jar" "${BASE}/api/v1/me" 2>/dev/null)
	if ! printf '%s' "$me" | grep -q "$want"; then
		printf 'the %s session does not answer /me as %s: %s\n' "$email" "$want" \
			"$(printf '%s' "$me" | head -c 200)" >&2
		exit 1
	fi
}
sign_in_for a 'outbox-a@qa.test' "$USER_A"
sign_in_for b 'outbox-b@qa.test' "$USER_B"
printf 'two tenant sessions established, each verified through /me\n'

as_a() { curl -fsS -b "/tmp/outbox-tenancy-a.jar" "$@"; }
as_b() { curl -fsS -b "/tmp/outbox-tenancy-b.jar" "$@"; }
as_root() { curl -fsS -b "/tmp/outbox-tenancy-root.jar" "$@"; }

echo
leg "the premise — tenant B's session resolves to tenant B and reaches the outbox"
# The body AND the status, because `curl -fsS` prints an empty string for a 403 and the leg
# then reads "did not reach the handler" — true, but silent about WHY, and the answer is either
# a permission refusal or a scope the guard could not resolve.
RAW=$(curl -sS -w '\n%{http_code}' -b /tmp/outbox-tenancy-b.jar "${BASE}/api/v1/notifications/outbox")
CODE=$(printf '%s' "$RAW" | tail -1)
BODY=$(printf '%s' "$RAW" | sed '$d')
if printf '%s' "$BODY" | grep -q 'retention_days'; then
	ok "B reached the handler and the body is the outbox"
else
	bad "B did not reach the handler (HTTP $CODE): $(printf '%s' "$BODY" | head -c 250)"
fi

echo
leg "a positive control — each tenant reads its OWN log and finds its own row"
# Both tenants, not one: a single tenant's control is also satisfied by a route that returns
# the same rows to everybody, which is exactly the shape the orgless leg below must not have.
A_BODY=$(as_a "${BASE}/api/v1/notifications/outbox")
B_BODY=$(as_b "${BASE}/api/v1/notifications/outbox")
A_OK=0; B_OK=0
printf '%s' "$A_BODY" | grep -q "$DELIVERY_A" && A_OK=1
printf '%s' "$B_BODY" | grep -q "$DELIVERY_B" && B_OK=1
if [ "$A_OK" -ne 1 ]; then
	bad "A cannot see its own tenant's row: $(printf '%s' "$A_BODY" | head -c 300)"
elif [ "$B_OK" -ne 1 ]; then
	bad "B cannot see its own tenant's row: $(printf '%s' "$B_BODY" | head -c 300)"
else
	ok "A finds row 1 and B finds row 2, each in its own tenant's log"
fi

echo
leg "neither tenant sees the other — the neighbour of the positive control"
CROSS=0
printf '%s' "$A_BODY" | grep -q "$DELIVERY_B" && CROSS=1
printf '%s' "$B_BODY" | grep -q "$DELIVERY_A" && CROSS=1
if [ "$CROSS" -eq 1 ]; then
	bad "a tenant's delivery log listed the other tenant's row"
else
	ok "neither tenant's log lists the other tenant's traffic"
fi

echo
leg "THE LEAK — the ORGLESS account reads PLATFORM traffic and no tenant's"
# The contract this slice fixes: an account with no organization is the platform operator, and
# `OutboxScope::Platform` is a deliberate capability — the rows stamped to no tenant. It is NOT
# the old `else { n.organization_id is null }` wildcard plus a tenant's rows: the platform arm
# is a complete predicate on its own, so neither tenant appears here.
BODY=$(as_root "${BASE}/api/v1/notifications/outbox")
LEAKED=0
printf '%s' "$BODY" | grep -q "$DELIVERY_A" && LEAKED=1
printf '%s' "$BODY" | grep -q "$DELIVERY_B" && LEAKED=1
if [ "$LEAKED" -eq 1 ]; then
	bad "the orgless account read a TENANT's delivery row (body starts: $(printf '%s' "$BODY" | head -c 120))"
elif printf '%s' "$BODY" | grep -q '"error":{'; then
	# **The ENVELOPE, not the key.** `OutboxRow` carries its own `error` field (the transport's
	# failure text), so a bare `grep '"error"'` matches every failed row — which is exactly what
	# this arm reads once the platform's own failed delivery is part of the fixture. The first
	# run of this leg failed on a healthy response for that reason, and reported "the orgless
	# account lost its platform log" about a body that was carrying the platform row.
	bad "the orgless account lost its platform log entirely: $(printf '%s' "$BODY" | head -c 160))"
else
	ok "the orgless account is answered with the platform audience, not a tenant's"
fi

echo
leg "the counts agree with the rows — a filtered list above unfiltered chips is a lie"
# One read, both halves: the answer carries `rows` and `counts`, so the chips that render above
# a list that is missing a tenant are the same response body. Asserted as a COMPARISON rather
# than two greps, because the defect is the disagreement and neither half is wrong alone.
BODY=$(as_b "${BASE}/api/v1/notifications/outbox")
ROW_COUNT=$(printf '%s' "$BODY" | grep -o '"id":"[0-9a-f-]\{36\}"' | wc -l | tr -d ' ')
TOTAL_SEEN=$(printf '%s' "$BODY" | grep -o '"total":[0-9]*' | head -1 | cut -d: -f2)
if [ -z "$TOTAL_SEEN" ]; then
	bad "the answer carries no counts at all: $(printf '%s' "$BODY" | head -c 200)"
elif [ "$ROW_COUNT" != "$TOTAL_SEEN" ]; then
	bad "B's list has $ROW_COUNT rows under chips that count $TOTAL_SEEN"
else
	ok "B's chips ($TOTAL_SEEN) count exactly the rows B's list returned ($ROW_COUNT)"
fi

echo
leg "THE RETRY BUTTON IS SCOPED — B cannot requeue tenant A's failed delivery by id"
# The write half. A retry is not a read: it hands the message back to the transport, so a
# cross-tenant retry lands in the other tenant's inbox. The row's own status is read back from
# PostgreSQL rather than believed from the response, because the response is the code under test.
# The status is read too. `curl -fsS` prints an empty body for a 500, and the first neutralised
# run of this gate answered 7/7 green with a 500 on exactly this leg: "the row is still failed"
# was true, and the conclusion drawn from it ("the retry was refused") was a server crash being
# read as a tenancy control working. A refusal is a REFUSAL — the answer has to be the endpoint's.
RAW=$(curl -sS -w '\n%{http_code}' -X POST -b /tmp/outbox-tenancy-b.jar \
	"${BASE}/api/v1/notifications/outbox/${DELIVERY_A}/retry")
CODE=$(printf '%s' "$RAW" | tail -1)
BODY=$(printf '%s' "$RAW" | sed '$d')
STATUS=$(psql "$OMNION_DATABASE_URL" -tAq \
	-c "select status from notification_deliveries where id = '$DELIVERY_A'" \
	2>/dev/null | tr -d ' \n')
# An EMPTY status is not a status. The first draft compared `[ "$STATUS" != "failed" ]`
# directly, so a failed psql read printed "B requeued tenant A's delivery" — reporting the
# exact opposite of what happened, from a query that never ran. The row's state is the whole
# evidence for this leg, so an unreadable row stops the run.
if [ -z "$STATUS" ]; then
	bad "A's delivery row could not be read, so this leg measured nothing"
elif [ "$CODE" != "200" ]; then
	# A 500 is a crash, not a refusal, and a crash still leaves the row untouched — which is
	# why the status check alone cannot tell the two apart.
	bad "the retry endpoint answered HTTP $CODE instead of refusing: $(printf '%s' "$BODY" | head -c 200)"
elif [ "$STATUS" != "failed" ]; then
	bad "B requeued tenant A's delivery — the row is now $STATUS"
elif printf '%s' "$BODY" | grep -q '"outcome":"requeued"'; then
	bad "the row is still failed but the endpoint claimed a requeue: $BODY"
else
	ok "the cross-tenant retry changed nothing (status is still $STATUS)"
fi

echo
leg "a TENANT sees the platform's own announcement — the second clause, and the capability"
# **The clause that `or n.organization_id is null` exists for.** Tick 75 made every row carry
# the RECIPIENT's tenant, so a platform announcement belongs to no tenant — and if the tenant arm
# selected only its own id, an administrator would read an EMPTY delivery log during an outage.
# This is the leg the neutralised control was blind to for a whole run: the platform row used to
# be inserted at the END, after every measurement, so no earlier leg could see it.
BODY=$(as_a "${BASE}/api/v1/notifications/outbox")
if printf '%s' "$BODY" | grep -q "$DELIVERY_P"; then
	ok "A reads the platform's announcement alongside its own row"
else
	bad "A cannot see platform traffic in its own log: $(printf '%s' "$BODY" | head -c 200)"
fi

echo
leg "the platform account still sees platform traffic — the capability the fix must not remove"
# Tick 75's fix stamps each row with the RECIPIENT's tenant; the platform's own announcement
# belongs to no tenant, so `OutboxScope::Platform` is the only scope that has this row — which
# is what makes refusing every orgless session the wrong fix.
BODY=$(as_root "${BASE}/api/v1/notifications/outbox")
if printf '%s' "$BODY" | grep -q "$DELIVERY_P"; then
	ok "the platform account reads platform traffic, and still no tenant's"
else
	bad "the platform account lost its platform traffic: $(printf '%s' "$BODY" | head -c 200)"
fi

summary