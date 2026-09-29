"use client";

/**
 * The security section's tab strip (REQ-012).
 *
 * The section grew past one screen, and the way it grew is the reason this file exists: the
 * posture overview's CSP, HSTS and referrer-policy rows each carry an action link, so
 * `/security/headers` was reachable from the overview but only from the overview. A screen that
 * is reachable from exactly one place is a screen an operator has to know exists.
 *
 * Two rules shape it:
 *
 * 1. **The strip lists the screens that exist, never the ones planned.** `ip-access` and
 *    `events` are slice 4; a tab that leads to "coming soon" is the dead control the
 *    definition of done forbids, so they are not here until they are. The tab count is
 *    therefore a claim the section can back. Slice 3 added `rate-limits` and
 *    `sign-in-protection` to the list, and it is worth naming *why* the rule held in the other
 *    direction too: the limiter's screen is reachable from the overview only once its route
 *    exists, so adding the tab and the screen in the same slice is what keeps the strip
 *    truthful. A tab added a commit before its screen would have been a lie for that commit.
 * 2. **The current tab is marked, not merely styled.** `aria-current="page"` is what a screen
 *    reader announces and what the walkthrough asserts against, so "which am I on" is never a
 *    question about a colour.
 */
import Link from "next/link";

/** The screens the section has, in the order an operator works them. */
const TABS = [
  { href: "/security", label: "Posture", key: "overview" },
  { href: "/security/findings", label: "Findings", key: "findings" },
  { href: "/security/headers", label: "Headers", key: "headers" },
  { href: "/security/rate-limits", label: "Rate limits", key: "rate-limits" },
  { href: "/security/sign-in-protection", label: "Sign-in protection", key: "sign-in-protection" },
] as const;

export type SecurityTabKey = (typeof TABS)[number]["key"];

export function SecurityTabs({ current }: { current: SecurityTabKey }) {
  return (
    <nav aria-label="Security section" data-security-tabs={current}>
      <ul className="flex flex-wrap gap-1 rounded-lg border border-line bg-surface p-1">
        {TABS.map((tab) => {
          const active = tab.key === current;
          return (
            <li key={tab.key}>
              <Link
                href={tab.href}
                aria-current={active ? "page" : undefined}
                data-security-tab={tab.key}
                className={`block rounded px-3 py-1.5 text-[12.5px] transition ${
                  active
                    ? "bg-accent-soft font-medium text-accent-strong"
                    : "text-muted hover:bg-quiet-soft hover:text-ink"
                }`}
              >
                {tab.label}
              </Link>
            </li>
          );
        })}
      </ul>
    </nav>
  );
}
