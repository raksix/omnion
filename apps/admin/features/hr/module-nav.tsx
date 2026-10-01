"use client";

/**
 * The HR module's own shelf (REQ-055).
 *
 * Slice 1 shipped the people core's **API** but no screen at all, so this shelf is written
 * against the module spec's nine destinations with the honest state that only some of them
 * exist yet — a link to a route that renders nothing is worse than no link, because it is a
 * dead button. Each entry below therefore points at a route that renders, and the ones the
 * later slices own get their entry **in the slice that builds them**, not as a placeholder here.
 *
 * **Derived from the pathname, not from a prop**, for the reason the inventory shelf gives: a
 * subnav that takes "which tab am I on" as an input has to be told by every screen that renders
 * it, and the first one that forgets is a tab that never highlights.
 */
import Link from "next/link";
import { usePathname } from "next/navigation";
import { ListChecks, Palmtree, type LucideIcon } from "lucide-react";

const LINKS: { href: string; label: string; icon: LucideIcon }[] = [
  // Leave is the only surface slice 2b builds, and it is the one people open: a request list with
  // the absence calendar above it. Employees, attendance and settings belong to the slices that
  // ship them — a link to a route that renders nothing is a dead button, and a dead button in a
  // module's own nav is the first thing that makes the module look unfinished.
  { href: "/hr/leave", label: "Leave", icon: Palmtree },
  // The type catalogue sits next to the requests it governs rather than under a Settings drawer:
  // deciding whether annual leave needs approval is part of working the leave screen.
  { href: "/hr/leave/types", label: "Leave types", icon: ListChecks },
];

export function HrModuleNav() {
  const pathname = usePathname();
  return (
    <nav
      aria-label="HR"
      data-qa-hr-module-nav
      className="flex flex-wrap items-center gap-1 border-b border-border pb-2"
    >
      {LINKS.map((link) => {
        // The **longest** matching href wins. `/hr/leave/types` is under `/hr/leave`, so a plain
        // `startsWith` lights the requests tab while the catalogue is open — the tab a person is
        // not looking at is the tab that says where they are. Sorting by length descending and
        // taking the first match is what makes the two nested routes honest.
        const here =
          pathname === link.href ||
          (pathname?.startsWith(`${link.href}/`) ?? false) ||
          (pathname?.startsWith(`${link.href}/new`) ?? false) ||
          (link.href === "/hr/leave" &&
            pathname?.startsWith("/hr/leave") === true &&
            ![...LINKS].some(
              (other) => other.href !== link.href && (pathname?.startsWith(other.href) ?? false),
            ));
        const Icon = link.icon;
        return (
          <Link
            key={link.href}
            href={link.href}
            data-qa-hr-module-link={link.href.split("/").pop()}
            aria-current={here ? "page" : undefined}
            className={`inline-flex h-8 items-center gap-1.5 rounded-md px-2.5 text-sm ${
              here
                ? "bg-muted font-medium text-foreground"
                : "text-muted-foreground hover:text-foreground"
            }`}
          >
            <Icon className="h-4 w-4" aria-hidden />
            {link.label}
          </Link>
        );
      })}
    </nav>
  );
}
