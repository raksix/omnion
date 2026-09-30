"use client";

/**
 * The tab bar every `/content-api/*` screen sits under (REQ-019).
 *
 * Four tabs, three routes so far — the Explorer and Usage tabs are slice 3 and are deliberately
 * **not** listed until they exist. A tab bar that links to a screen that will 404 is the panel's
 * own "coming soon", which the Definition of Done forbids, and an empty tab is worse than a
 * missing one: it advertises a feature that cannot be reached.
 *
 * The active tab is derived from the pathname rather than passed in, so a new route that forgets
 * the prop still highlights correctly — the prop version of the same fact is a second answer that
 * eventually disagrees.
 */
import { FileText, Gauge, KeyRound } from "lucide-react";
import Link from "next/link";
import { usePathname } from "next/navigation";
import type { ReactNode } from "react";

/** The routes of the section, in the order a person works through them. */
export const CONTENT_API_NAV = [
  {
    href: "/content-api",
    label: "Tokens",
    icon: KeyRound,
    /** Longest matching prefix, so `/content-api/explorer` does not light up the Tokens tab. */
    exact: true,
  },
  {
    href: "/content-api/docs",
    label: "Docs",
    icon: FileText,
    exact: false,
  },
] as const;

/** The frame a Content API screen renders inside. */
export function ContentApiShell({ children }: { children: ReactNode }) {
  const pathname = usePathname();

  return (
    <div className="flex flex-col gap-4">
      <nav
        aria-label="Content API screens"
        className="flex flex-wrap items-center gap-1.5"
        data-content-api-nav
      >
        <span className="mr-1 flex items-center gap-1.5 rounded-lg bg-quiet-soft px-2 py-1 text-[12px] font-medium text-ink">
          <Gauge className="size-3.5 shrink-0" aria-hidden />
          Content API
        </span>
        {CONTENT_API_NAV.map((entry) => {
          const current = entry.exact ? pathname === entry.href : pathname.startsWith(entry.href);
          const Icon = entry.icon;
          return (
            <Link
              key={entry.href}
              href={entry.href}
              aria-current={current ? "page" : undefined}
              data-content-api-tab={entry.label.toLowerCase()}
              className={`flex items-center gap-1.5 rounded-lg px-2.5 py-1.5 text-[12.5px] transition ${
                current
                  ? "bg-accent-soft font-medium text-accent-strong"
                  : "text-muted hover:bg-quiet-soft hover:text-ink"
              }`}
            >
              <Icon className="size-3.5 shrink-0" aria-hidden />
              {entry.label}
            </Link>
          );
        })}
      </nav>
      {children}
    </div>
  );
}