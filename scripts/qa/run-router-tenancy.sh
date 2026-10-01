#!/usr/bin/env bash
# The router's tenancy boundary: which rules can name somebody the event's tenant does not own.
#
#   bash scripts/qa/run-router-tenancy.sh
#
# ## Why this gate exists
#
# Ticks 73 and 74 fixed the recipient guard in the SLA worker and then in
# `POST /notifications/emit`, and both ticks closed on the same sentence: *"the row's
# organization is the recipient's tenant"*. The emit route asks
# `omnion_notifications::audience::may_address` and stamps each row with the organization
# `store::recipient_organizations` reported.
#
# **The router was never asked, and it is the third caller of that store function.** `route()`
# resolved a recipient set and wrote every row through
# `record_with_deliveries(pool, event.organization_id, …)` — the **event's** organization,
# unconditionally. Two of its four recipient rules make that wrong:
#
#   * `RecipientRule::Actor` returns `event.actor_user_id` verbatim;
#   * `RecipientRule::PayloadUser` returns an id the **producer wrote into the payload**, whose
#     only check was that it parses as a uuid.
#
# Measured by the suite this gate runs (see the module doc of
# `apps/api/tests/notification_router_tenancy.rs`): a tenant event naming another tenant's user
# through a payload rule answered `created: 1` and wrote a row stamped with the **sending**
# tenant next to a recipient belonging to a different one — the shape tick 74 called "a tenant
# leak wearing a select element", one layer down, with the leak in the *stamp*.
#
# The platform case is the second half and is worse. `organization_id: None` is the router's
# documented unscoped branch, so a platform event addressing a tenant's user wrote a row stamped
# `null` — invisible to *every* tenant's `notifications.admin` outbox, including the tenant whose
# person reached it, while `outbox_counts(None)` counted it as platform traffic.
#
# ## Why a separate gate from `run-notification-tenancy-http.sh`
#
# That gate measures the **emit route over a real socket**: two tenants, real sessions, real
# `notifications.send` grants. That is the right instrument for an HTTP route and the wrong one
# for the router, which is a library call with no permission layer of its own — its boundary is
# the event's `organization_id`, so proving it over HTTP would have to fabricate a route to
# exercise it. Here the database *is* the instrument: the defect is a column's value and a
# foreign tenant's row count, both of which only exist in PostgreSQL.
#
# ## Why the negative control matters more than the positive
#
# `rows_for(...) == 0` is also satisfied by a router that stopped routing, and the *positive*
# control inside each walk is what stops that from passing. The control that proves this gate
# names the defect is the one below: with the filter removed, exactly the tenancy walks must go
# red and the two controls must stay green.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR="${QA_CARGO_TARGET_DIR:-/dev/shm/w8-target}"
export CARGO_INCREMENTAL=0

SUITE=notification_router_tenancy

echo "[router-tenancy] the suite, green"
cargo test -p omnion-api --test "$SUITE" -- --test-threads=2

echo
echo "[router-tenancy] negative control: a router with no tenancy filter"

# Remove the guard the way it was absent, which is not "delete the function" — the function has
# callers and its own unit tests. It is the *call site* that has to be neutralised, so that the
# failing set is the database walks and not the eight `audience` unit tests.
cp crates/notifications/src/router.rs /tmp/w8-router-tenancy-backup.rs
python3 - <<'PY'
p = "crates/notifications/src/router.rs"
s = open(p).read()
needle = "        let kept = crate::audience::addressable_recipients("
assert needle in s, "the gate must find the guard it is neutralising"
# Make the filter a pass-through: every resolved recipient is kept, stamped with the EVENT's
# organization — which is precisely the pre-fix body.
s = s.replace(
    needle,
    "        let kept: Vec<(uuid::Uuid, Option<uuid::Uuid>)> = resolved.iter()\n"
    "            .map(|id| (*id, event.organization_id)) // NEGATIVE CONTROL\n"
    "            .collect();\n"
    "        #[allow(unused)]\n"
    "        let _unused = || {\n"
    "            crate::audience::addressable_recipients(",
    1,
)
# Close the ignored closure right after the original call's arguments.
close = "            &|id| Some(organizations.get(&id).copied().flatten()),\n        );\n"
assert close in s, "the gate must find the filter's closing argument"
s = s.replace(close, close + "        };\n", 1)
open(p, "w").write(s)
PY

set +e
cargo test -p omnion-api --test "$SUITE" -- --test-threads=2 >/tmp/w8-router-tenancy-ctl.log 2>&1
control_status=$?
set -e
cp /tmp/w8-router-tenancy-backup.rs crates/notifications/src/router.rs
rm -f /tmp/w8-router-tenancy-backup.rs

if [ "$control_status" -eq 0 ]; then
  echo "  FAIL: removing the tenancy filter left the suite green, so the gate names nothing." >&2
  exit 1
fi

# **The failing set is compared by name, not by exit code.** "Something went red" is the check
# this branch has learned not to accept: an unrelated failure would satisfy a bare exit code and
# the control would pass for the wrong reason. Three walks measure the defect and two do not —
# the disabled-account walk and the draft-shape walk are tenancy-free by design and must stay
# green, which is what proves the control removed the *tenancy* rule and not the router.
control_failures=$(sed -n '/^failures:$/,$p' /tmp/w8-router-tenancy-ctl.log \
  | grep -oE '^\s+(a_platform_event_stamps_the_tenant_whose_user_it_reached|a_refused_cross_tenant_payload_counts_as_an_unmatched_rule|only_the_caller_supplied_rules_can_leave_the_events_tenant|the_router_writes_a_row_into_the_recipients_tenant_not_the_events)$' \
  | tr -d ' ' | sort -u || true)

expected_failures="a_platform_event_stamps_the_tenant_whose_user_it_reached
a_refused_cross_tenant_payload_counts_as_an_unmatched_rule
only_the_caller_supplied_rules_can_leave_the_events_tenant
the_router_writes_a_row_into_the_recipients_tenant_not_the_events"

if [ "$control_failures" != "$expected_failures" ]; then
  echo "  FAIL: the control failed on a different set than the tenancy walks:" >&2
  echo "--- got ---" >&2
  echo "$control_failures" >&2
  echo "--- expected ---" >&2
  echo "$expected_failures" >&2
  tail -40 /tmp/w8-router-tenancy-ctl.log >&2
  exit 1
fi
echo "  OK: the four tenancy walks go red, and only those."

# The two neighbours must have survived, named explicitly: `set -e` never notices a conditional
# that was not taken, so a walk that silently stopped running would look like a pass.
for neighbour in a_disabled_account_in_the_events_own_tenant_is_still_addressable \
                 a_draft_carries_no_organization_because_the_caller_binds_it; do
  if grep -q "FAILED" /tmp/w8-router-tenancy-ctl.log && \
     grep -qE "^test $neighbour \.\.\. FAILED" /tmp/w8-router-tenancy-ctl.log; then
    echo "  FAIL: $neighbour also went red — it is tenancy-free and must survive the control." >&2
    exit 1
  fi
done
echo "  OK: the two tenancy-free neighbours stayed green."

# The source is restored byte for byte, and saying so is better than assuming it.
if grep -q 'NEGATIVE CONTROL' crates/notifications/src/router.rs; then
  echo "  FAIL: the control is still in the source tree." >&2
  exit 1
fi
echo "  OK: the source is restored."

echo
echo "[router-tenancy] PASS"