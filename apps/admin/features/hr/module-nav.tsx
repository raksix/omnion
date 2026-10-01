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
import {
  Clock,
  FileBarChart,
  FileText,
  ListChecks,
  Network,
  Palmtree,
  Users,
  type LucideIcon,
} from "lucide-react";

const LINKS: { href: string; label: string; icon: LucideIcon }[] = [
  // Employees is the module's front door (slice 1). It is first because everything else in HR is a
  // row that points at a person: leave, attendance and onboarding all ask "which employee?" before
  // they ask anything else, so this is the tab the rest of the module hangs off.
  { href: "/hr/employees", label: "Employees", icon: Users },
  // The department tree and the org chart share a screen, because they answer one question
  // between them — "who is in this part of the organization, and who leads it".
  { href: "/hr/departments", label: "Departments", icon: Network },
  // Leave is the surface people open most, and the request list with the absence calendar above it
  // is where a request is actually made and decided.
  { href: "/hr/leave", label: "Leave", icon: Palmtree },
  // The type catalogue sits next to the requests it governs rather than under a Settings drawer:
  // deciding whether annual leave needs approval is part of working the leave screen.
  { href: "/hr/leave/types", label: "Leave types", icon: ListChecks },
  // Attendance (slice 2d). The roster is the operator's morning screen, so it sits in the module
  // shelf rather than behind My workspace -- the employee's own clock is the self-service route,
  // and this one answers for everybody at once behind `hr.attendance.read`.
  { href: "/hr/attendance", label: "Attendance", icon: Clock },
  // Documents (slice 4b) is the filing cabinet across everybody, so it sits with the operator's
  // screens rather than under My workspace: it answers "whose expires next", which is a question
  // about the organization and not about the person asking it.
  { href: "/hr/documents", label: "Documents", icon: FileText },
  // Reports last, because it is the screen you arrive at when you already know what you want.
  { href: "/hr/reports", label: "Reports", icon: FileBarChart },
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
        // The **longest** matching href wins, and the match is a segment boundary. A plain
        // `startsWith` lights the requests tab while the catalogue is open — the tab a person is
        // not looking at is the tab that says where they are — and it also lights `/hr/leave`
        // for a route that has nothing to do with leave. Sorting descending and taking the first
        // match that is either the pathname itself or a **segment** prefix is what makes nested
        // routes honest.
        const here = [...LINKS]
          .sort((a, b) => b.href.length - a.href.length)
          .find((candidate) => {
            const path = pathname ?? "";
            return path === candidate.href || path.startsWith(`${candidate.href}/`);
          });
        const Icon = link.icon;
        return (
          <Link
            key={link.href}
            href={link.href}
            data-qa-hr-module-link={link.href.split("/").pop()}
            aria-current={here?.href === link.href ? "page" : undefined}
            className={`inline-flex h-8 items-center gap-1.5 rounded-md px-2.5 text-sm ${
              here?.href === link.href
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
