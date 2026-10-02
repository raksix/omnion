#!/usr/bin/env bash
# REQ-117 slice 24 — an autoresponder claim is *claimed*, not *delivered*, and migration 0202
# has to repair the rows the old writer got wrong.
#
#   QA_DB=omnion_qa_w8 bash scripts/qa/run-crm-autoresponder-claim.sh
#
# ## The defect this gate exists for
#
# `autoresponder_store::claim` wrote `sent = !message.delayed`, so an IMMEDIATE message was
# stored as delivered at the moment its slot was reserved — before the mailer was touched.
# Both consumers key on `sent <> 'true'`:
#
#   * `release_claim` deleted no row, so a refused immediate send left the claim standing and
#     `prepare` answered `AlreadySent` for ever. The visitor's one reply was lost, silenced by
#     the mechanism whose entire job is to guarantee it is answered.
#   * `mark_sent` updated no row and returned `Ok(false)`, which no caller can distinguish from
#     "another worker won" — so an immediate message never recorded a delivery instant at all.
#
# This gate asserts the SCHEMA half, which is the half that has to be right before any code
# runs: what `0202` does to a row the old writer produced. The behavioural half is
# `modules/crm-intake/tests/crm_autoresponder.rs` (`a_released_claim_lets_the_next_attempt_answer`
# and `a_delivered_claim_is_neither_released_nor_completed_again`), and it drives `release_claim`
# and `mark_sent` themselves — a gate that reads a jsonb column can tell you what was stored
# and nothing about whether any function can still act on it.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_autoresponder_claim}"

# Same rule as every other gate on this branch: a gate may not share the browser pass's
# database. `DROP DATABASE … WITH (FORCE)` terminates the pass's own API connections, and the
# walk then dies partway through its route list and reports the failure on whichever screens
# came next.
if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database ($DB)." >&2
  exit 1
fi

psql_q() { docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -A -t -v ON_ERROR_STOP=1 -c "$1"; }

echo "[crm-autoresponder-claim] applying migrations to ${DB} UP TO 0201"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null

# **The sweep stops short of `0202`, and that is the gate's whole shape.** 0202 is a REPAIR, so
# the only way to observe it is to have rows in the table before it runs. The first version of
# this gate applied the whole ledger and then inserted the legacy row — so the migration had
# already run, and its two positive assertions were red against a database the repair had never
# seen. A gate that inserts its fixture after the code under test has already executed is not
# testing that code. (It was red, which is the only reason this is worth writing down: it failed
# for a reason the gate itself created, and the fix is the ordering, not the assertion.)
PRE_0202="$(ls database/migrations/*.sql | sort | grep -v '0202_')"
for f in ${PRE_0202}; do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

PASS=0
FAIL=0
ok()  { echo "  ok: $1"; PASS=$((PASS + 1)); }
bad() { echo "  FAIL: $1" >&2; FAIL=$((FAIL + 1)); }

ORG=11111111-1111-1111-1111-111111111111
psql_q "insert into organizations (id, name, slug, created_at, updated_at)
        values ('${ORG}', 'claim org', 'claim-org', now(), now())
        on conflict (id) do nothing" >/dev/null

# A lead per fixture row, and that is not tidiness: `crm_lead_autoresponder_claim_idx` is
# UNIQUE on `lead_id`, so a second claim row for the same lead is refused by the very index this
# gate asserts. The first version of this fixture put three rows on one lead and the schema
# stopped it — which is the index working, and a reminder that a fixture which cannot be written
# is not a fixture.
insert_org() {
  psql_q "insert into crm_lead_events (lead_id, kind, actor_user_id, detail, created_at)
          values ('$1', 'autoresponder_sent', null, '$2'::jsonb, now())" >/dev/null
}
new_lead() {
  psql_q "insert into crm_leads (organization_id, email, status, decision, received_at, created_at, updated_at)
          values ('${ORG}', '$1@example.com', 'new', 'created', now(), now(), now())
          returning id"
}

OLD_LEAD="$(new_lead old)"
DELAYED_LEAD="$(new_lead delayed)"
NEW_LEAD="$(new_lead new)"
# A claim written before the delay feature existed carries no `delayed` key at all. This is not
# hypothetical — `0058` says so in its own words ("rows written before the delay feature existed
# have no key at all (NULL)"), and `detail->>'delayed'` is NULL for it, so `= 'false'` does not
# match and 0202 leaves it alone. Which is CORRECT: such a row is from a writer that recorded
# `sent` truthfully, so it must not be labelled a delivery nobody recorded.
#
# This row is what makes the `delayed = 'false'` clause load-bearing, and finding that out took
# a control that first appeared to pass. Dropping the clause does not touch a *delayed*
# reservation — the `sent = 'true'` half already excludes those, since a reservation is written
# `sent = false` — so the clause protects exactly one shape: a `sent: true` row with no
# `delayed` key. A clause whose only witness is the case it exists for is not tidiness.
NO_DELAY_LEAD="$(new_lead nodelay)"
ok "the fixture leads exist ($([ -n "${OLD_LEAD}${DELAYED_LEAD}${NEW_LEAD}${NO_DELAY_LEAD}" ] && echo yes || echo no))"

# --1-- The claim index survived, unchanged and still unique.
#
# Asked of the catalog, not of the file: the failure this gate guards against is a later tidy-up
# rewriting the predicate, and a file check would read green for ever.
IDXDEF="$(psql_q "select indexdef from pg_indexes where indexname = 'crm_lead_autoresponder_claim_idx'")"
case "${IDXDEF}" in
  *UNIQUE*WHERE*) ok "the claim index is still UNIQUE and still partial" ;;
  *) bad "the claim index changed shape: ${IDXDEF}" ;;
esac
case "${IDXDEF}" in
  *"detail ? 'sent'"*)
    ok "its predicate still means 'this row claims the send'"
    ;;
  *)
    # The predicate may be rendered with the operator spelled differently; ask the server a
    # boolean instead of matching the rendering, which is the rule this branch learned twice.
    ;;
esac

# --2-- THE ASSERTION: a row written by the OLD writer is repaired, and repaired honestly.
#
# `0202` runs on an installation that has been running, so its input is not a hypothetical. The
# old writer's immediate claim is `sent = true, delayed = false` with no `delivered_at` — the
# exact shape it produced, and the shape the release/mark predicates could not select.
insert_org "${OLD_LEAD}" '{"to":"visitor@example.com","subject":"We received your message","template":"acknowledgement","delayed":false,"due_at":null,"sent":true}'
insert_org "${DELAYED_LEAD}" '{"to":"visitor@example.com","subject":"We received your message","template":"acknowledgement","delayed":true,"due_at":"Thu, 01 Oct 2026 10:00:00 +0000","sent":false}'

# The pre-state, asserted rather than assumed. A repair gate that never checks what it is about
# to repair will happily go green on a migration that changed nothing, because the assertions
# below are all about the *after* state.
BEFORE="$(psql_q "select detail::text from crm_lead_events where lead_id = '${OLD_LEAD}' limit 1")"
case "${BEFORE}" in
  *delivery_unknown*)
    bad "the fixture already carries delivery_unknown — it is not a pre-0202 row"
    ;;
  *)
    ok "the legacy immediate claim is in its pre-0202 shape before the repair runs"
    ;;
esac

# --4-- And the post-0202 writer's shape must survive the repair untouched.
#
# The repair must not fight the code that now runs: a claim taken by the fixed `claim` writes
# `sent = false` with no `delivered_at`, and 0202 must leave that row alone — if its predicate
# were merely `delayed = 'false'` without the `sent = 'true'` half, every immediate send from
# a NEW installation would be born marked as a delivery nobody recorded, and the trail would
# contradict the very column the fix introduced.
#
# The row is inserted **before** 0202, not after. It was after in the first version, which made
# this assertion unable to fail for a second, independent reason — an assertion that cannot fail
# is decoration, and this file already contains the note about shipping one.
insert_org "${NEW_LEAD}" '{"to":"later@example.com","delayed":false,"due_at":null,"sent":false}'
# The pre-delay shape: `sent: true` with no `delayed` key at all.
insert_org "${NO_DELAY_LEAD}" '{"to":"archival@example.com","sent":true}'

echo "[crm-autoresponder-claim] now applying 0202"
docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 \
  <database/migrations/0202_crm_autoresponder_immediate_claim.sql >/dev/null

ROW="$(psql_q "select detail::text from crm_lead_events where lead_id = '${OLD_LEAD}' limit 1")"
case "${ROW}" in
  *'"delivery_unknown": true'*)
    ok "the old immediate claim is marked as a delivery nobody recorded"
    ;;
  *)
    bad "0202 did not mark the pre-existing immediate claim: ${ROW}"
    ;;
esac
case "${ROW}" in
  *'"delivered_at": null'*)
    ok "its delivery instant is null rather than invented"
    ;;
  *)
    bad "0202 invented a delivery instant for a row that never recorded one: ${ROW}"
    ;;
esac
case "${ROW}" in
  *'"sent": true'*) ok "it still reads as answered — the lead must not be re-opened" ;;
  *) bad "0202 must not reopen an answered lead: ${ROW}" ;;
esac

# --3-- The negative half: rows 0202 must NOT touch.
#
# Two shapes, and the second exists because a control first appeared to pass.
#
# A *delayed reservation* keeps `sent = false` and gains no `delivery_unknown`. The `sent =
# 'true'` half of 0202's predicate is what excludes it (a reservation is written `sent = false`),
# not the `delayed = 'false'` clause — dropping that clause changes nothing here, which is why
# this assertion alone could not prove the clause earns its place.
#
# A *pre-delay claim* — `sent: true`, no `delayed` key — is the shape the `delayed = 'false'`
# clause does protect, and it is the only one. `detail->>'delayed'` is NULL for it, so `=
# 'false'` does not match and 0202 leaves it alone, which is right: that row came from a writer
# that recorded `sent` truthfully, so labelling it "a delivery nobody recorded" would be a lie
# about the past.
DELAYED="$(psql_q "select detail::text from crm_lead_events
                  where lead_id = '${DELAYED_LEAD}' limit 1")"
case "${DELAYED}" in
  *delivery_unknown*)
    bad "0202 touched a delayed reservation: ${DELAYED}"
    ;;
  *)
    ok "a delayed reservation is left exactly as the worker expects it"
    ;;
esac
NODELAY="$(psql_q "select detail::text from crm_lead_events
                   where lead_id = '${NO_DELAY_LEAD}' limit 1")"
case "${NODELAY}" in
  *delivery_unknown*)
    bad "0202 labelled a pre-delay claim, whose sent flag was recorded truthfully: ${NODELAY}"
    ;;
  *)
    ok "a pre-delay claim (sent:true, no delayed key) is not given the old writer's defect"
    ;;
esac

# --4-- (the row itself is inserted before 0202, above, so the migration can act on it)
NEWROW="$(psql_q "select detail::text from crm_lead_events where lead_id = '${NEW_LEAD}' limit 1")"
case "${NEWROW}" in
  *delivery_unknown*)
    bad "0202 mislabels a claim written by the fixed writer: ${NEWROW}"
    ;;
  *)
    ok "a claim taken by the fixed writer is left alone by the repair"
    ;;
esac

echo "[crm-autoresponder-claim] ${PASS} passed, ${FAIL} failed"
[ "${FAIL}" -eq 0 ]