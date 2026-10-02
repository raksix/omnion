#!/usr/bin/env bash
# Does a focused pass actually run the section it was pointed at?
#
# The bug this proves absent: every section's depth passes were guarded by
# `!onlyGroup(group)`, which is TRUE for every section the pass did NOT name. So `--only=hr`
# walked the fourteen `/hr` routes, ran zero HR depth passes, and spent its minutes on CRM,
# sales and accounting instead -- then printed a normal coverage line and a clean report. A
# full pass was worse still: it skipped HR and inventory outright. The routes counter and the
# depth-pass counter cannot see each other, so nothing reported the gap.
#
# Each check below is a red/green check on the SOURCE, not on a pass: a browser pass is minutes
# long and proves the harness by running it, which is the same instrument that hid the bug.
#
# Two rules for editing this file (both learned the hard way):
#   * `set -o pipefail` + `awk ... | grep -q` turns SIGPIPE (141) into a failed pipeline, so a
#     passing check reads red. Write to a file first, or use `grep -c`.
#   * A check that cannot fail is worse than no check: every assertion below is shown red
#     against the pre-fix commit, and the whole file is run both ways.
set -uo pipefail
cd "$(dirname "$0")/../.."
F=scripts/qa/walkthrough.cjs
TMP=$(mktemp)
trap 'rm -f "$TMP"' EXIT
pass=0; fail=0
ok()   { pass=$((pass+1)); printf '  ok   %s\n' "$1"; }
bad()  { fail=$((fail+1)); printf '  FAIL %s\n' "$1"; }
check() { # name, expected-count, pattern
  local name=$1 want=$2 pat=$3 got
  got=$(grep -cF "$pat" "$F" 2>/dev/null || true)
  got=${got:-0}
  if [ "$got" = "$want" ]; then ok "$name ($got)"; else bad "$name (want $want, got $got)"; fi
}

echo "== the polarity itself =="
# The helper must exist, and every section guard must use it.
check 'runsSection helper defined' 1 'const runsSection = (group) => ONLY_ALL || onlyGroup(group);'
# An inverted guard is the defect itself. Exactly THREE may remain, and each is legitimate:
# they wrap the *other* sections (the media/IAM/analytics passes, the sales passes, the
# sign-out + mobile + renderer tail) so a CRM-scoped pass skips them. Anything above three is
# an inverted section guard; anything that names a section's OWN passes is the bug.
# The pattern is the whole guard, quotes included, so the prose in `runsSection`'s doc comment --
# which quotes the old spelling to explain it -- is not counted as a guard.
check 'inverted guards remaining' 3 'if (!onlyGroup("'
# The five sections each have to be guarded by the fixed helper.
check 'hr guards'         6 'if (runsSection("hr"))'
check 'sales guards'      1 'if (runsSection("sales"))'
check 'accounting guards' 1 'if (runsSection("accounting"))'
check 'crm guards'        1 'if (runsSection("crm"))'

echo "== every section is in the scoped table =="
# A section missing from this table walks zero routes under --only=<section> and reports
# empty-pass, which is how `--only=accounting` shipped its own finding.
check 'scoped table names accounting' 1 'accounting: "/accounting"'
check 'scoped table names hr'          1 'hr: "/hr"'

echo "== a focused pass counts what it drove =="
check 'per-section pass table defined' 1 'const SECTION_PASSES = {'
check 'coverage check pushed'          1 'section-drove-nothing'

echo "== the mobile pass visits real screens =="
# A merge into this branch replaced all 33 entries with 34 `{ path: "path", name: "name" }`
# placeholders, so the phone pass had been screenshotting the address /path ever since.
check 'placeholder routes remaining' 0 'path: "path", name: "name"'
check 'mobile routes declared'       1 'const mobileRoutes = ['
for n in hr-me-leave crm-contacts health-metrics; do
  if grep -qF "name: \"$n\"" "$F"; then ok "mobile route $n"; else bad "mobile route $n"; fi
done

echo "== the shipped screens are in the inventory =="
for r in /inventory/transfers /inventory/stocktake /inventory/alerts \
         /accounting/journal /accounting/accounts /accounting/reports \
         /accounting/invoices /accounting/payments; do
  if grep -qF "path: \"$r\"," "$F"; then ok "route $r"; else bad "route $r"; fi
done

echo
echo "probe: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
