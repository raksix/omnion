"use client";

/**
 * The panel's frame: a sidebar with the sections, a sticky header with the current screen and
 * the site switcher, and the screen itself. Below `lg` the sidebar becomes a drawer.
 */
import { useState, type ReactNode } from "react";

import { Activity, ArrowUpCircle, BarChart3, Bell, BellRing, Blocks, Bot, Boxes, CalendarClock, ClipboardCheck, Code2, Database, FileCode2, FileDown, FileText, Fingerprint, Gauge, Globe, HardDriveDownload, HeartPulse, Images, Import, KeyRound, LayoutDashboard, LockKeyhole, LogOut, Menu, Package, Radio, Route, Scale, ScrollText, ShieldCheck, SlidersHorizontal, Sparkles, Timer, UserCog, UsersRound, Waypoints, Webhook, X } from "lucide-react";
import Link from "next/link";
import { usePathname, useRouter } from "next/navigation";

import { SiteSwitcher } from "@/components/site-switcher";
import { GlobalSearch } from "@/components/global-search";
import { NotificationBell } from "@/components/notification-bell";
import { useDeveloperAccess } from "@/lib/developer-access";
import { useSession } from "@/lib/session";

const NAV = [
  { href: "/", label: "Overview", icon: LayoutDashboard },
  { href: "/pages", label: "Pages", icon: FileText },
  { href: "/media", label: "Media", icon: Images },
  // Backups sit beside Media rather than under Settings: an operator asking "where are my
  // files and can I get them back" is one question, and burying half of it under a
  // settings sub-path is what makes somebody believe the platform has no restore point.
  { href: "/backups", label: "Backups", icon: HardDriveDownload },
  // System health (REQ-014, slice 1). It sits with Backups rather than under Settings for
  // the same reason: "can I get my data back" and "is anything answering" are both questions
  // an operator asks at the same moment, usually while something is already wrong — and
  // burying the liveness screen under a settings sub-path is how a platform looks healthy
  // to the person who opened the admin panel to find out that it is not.
  { href: "/health", label: "System Health", icon: HeartPulse },
  { href: "/analytics", label: "Analytics", icon: BarChart3 },
  { href: "/notifications", label: "Notifications", icon: Bell },
  // The event console (REQ-016, slice 1). It sits beside Notifications rather than under
  // Settings because both answer the same question from the bus's side — "what does the
  // platform think happened" and "who was told" — and an operator chasing a missing webhook
  // needs both on the same shelf.
  { href: "/events", label: "Events", icon: Activity },
  // The endpoints and their delivery history (REQ-016, slice 2). It sits next to Events
  // rather than under Settings because the two are the same investigation from both ends:
  // the feed says what happened, this says who was told and whether they got it.
  { href: "/webhooks", label: "Webhooks", icon: Webhook },
  { href: "/sites", label: "Sites", icon: Globe },
  { href: "/ai", label: "AI Hub", icon: Sparkles },
  // The observability centre (REQ-126) and its six areas. It sits beside Events rather than
  // under Settings: Events answers "what does the platform think happened", this answers "what
  // is it doing right now and what did it drop" — and both are read during the same incident.
  // The parent links to the overview rather than to any one area, because the landing screen is
  // the one that knows which area holds the answer; deep-linking an operator into the log
  // explorer when they asked "is anything wrong" is the wrong default.
  { href: "/observability", label: "Observability", icon: Gauge },
  { href: "/observability/logs", label: "Observability logs", icon: ScrollText },
  { href: "/observability/traces", label: "Traces", icon: Route },
  { href: "/observability/metrics", label: "Metric catalogue", icon: BarChart3 },
  { href: "/observability/exporters", label: "Exporters", icon: Radio },
  { href: "/observability/alerts", label: "Alert rules", icon: BellRing },
  { href: "/observability/settings", label: "Observability settings", icon: SlidersHorizontal },
  // The deployment centre's release surface (REQ-128, slice 4). It sits beside System Health
  // rather than under Settings because the three answers are the same operator question asked at
  // three moments: "what am I running" (artifacts), "how do I install this somewhere else"
  // (install) and "how do I get to the next version, and what does it cost me if I have to go
  // back" (upgrade). An upgrade helper filed under a settings sub-path is a helper nobody opens
  // at 2am, which is exactly when it is needed.
  { href: "/deployment/artifacts", label: "Release artifacts", icon: Package },
  { href: "/deployment/install", label: "Install bundle", icon: Boxes },
  { href: "/deployment/upgrade", label: "Upgrade helper", icon: ArrowUpCircle },
  // The migration ledger (REQ-129, slice 1) belongs beside them, not under Settings: an operator
  // asking "what changed my database" is mid-deploy, and the answer they need is the pending set
  // and the rehearsed/unrehearsed column — the same three questions the other two answer.
  { href: "/deployment/migrations", label: "Migration ledger", icon: Database },
  // Anonymised exports sit with the ledger rather than under Settings: a support dump is asked
  // for in the middle of an incident, and the two questions beside it are the same one — "what
  // does this installation know, and what may it hand over".
  { href: "/deployment/exports", label: "Anonymised exports", icon: FileDown },
  // The developer portal's GraphQL surface (REQ-130, slice 2). It sits beside the deployment
  // release surface rather than under a settings sub-path because both answer the same integrator
  // question — "what can a client of this installation do, and what does it cost me" — and an
  // integrator never goes looking under Settings for it. The playground is the entry, the registry
  // beside it, and the schema explorer under it because those two are read together.
  { href: "/developer/graphql", label: "GraphQL playground", icon: Waypoints },
  { href: "/developer/graphql/documents", label: "Persisted documents", icon: FileCode2 },
  { href: "/developer/graphql/schema", label: "GraphQL schema", icon: Blocks },
  { href: "/developer/graphql/settings", label: "GraphQL settings", icon: SlidersHorizontal },
  // The versioned API policy (REQ-130, slice 4). Beside the GraphQL settings rather than
  // under them: a sunset is a DATE an integrator reads, and Settings is where nobody goes
  // looking for a deadline. The screen and the response headers read the same rows, so an
  // operator who changes a date here has changed what the API says.
  { href: "/developer/api/deprecations", label: "API deprecations", icon: CalendarClock },
  { href: "/settings/iam", label: "Identity & access", icon: ShieldCheck },
  { href: "/settings/iam/users", label: "Users", icon: UserCog },
  { href: "/settings/iam/groups", label: "Groups", icon: UsersRound },
  { href: "/settings/iam/service-accounts", label: "Service accounts", icon: Bot },
  { href: "/settings/iam/simulator", label: "Simulator", icon: Scale },
  { href: "/settings/iam/policies", label: "Policies", icon: ScrollText },
  { href: "/settings/iam/approvals", label: "Approvals", icon: ClipboardCheck },
  { href: "/settings/iam/authentication", label: "Authentication", icon: KeyRound },
  { href: "/settings/iam/provisioning", label: "Provisioning", icon: Import },
  { href: "/settings/iam/roles", label: "Roles", icon: ShieldCheck },
  { href: "/settings/iam/security", label: "Security", icon: LockKeyhole },
  { href: "/settings/iam/sessions", label: "Sessions", icon: Timer },
  { href: "/settings/iam/devices", label: "Devices", icon: Fingerprint },
  { href: "/settings/search", label: "Search settings", icon: SlidersHorizontal },
  // The developer portal (REQ-022, slice 2). Three entries rather than eight: the brief lists
  // eight, but OAuth apps, plugins, themes, docs and the sandbox are slices 3 and 4, and a nav
  // link to a screen that does not exist is the dead control the definition of done forbids.
  // These three are the whole of what slice 2 ships.
  { href: "/developer", label: "Developer", icon: Code2, needsDeveloper: true },
  { href: "/developer/api-keys", label: "API keys", icon: KeyRound, needsDeveloper: true },
  { href: "/developer/logs", label: "Request log", icon: ScrollText, needsDeveloper: true },
] as const;

/**
 * A navigation entry that only exists for accounts allowed into the developer portal.
 *
 * The property is on the entry rather than in a filter above, so "which entries are conditional"
 * is one list a reader can scan instead of a second list somewhere else that has to be kept in
 * step with it.
 */
type NavItem = (typeof NAV)[number];

/**
 * Whether an entry is shown to this account.
 *
 * `needsDeveloper` is resolved against a route the API guards for `developer.read`, and the
 * answer is `null` while it is in flight — which means the group is hidden for that first paint
 * and appears a moment later. That is the right trade: a group that appears, then vanishes, then
 * reappears as the answer lands is a flicker, and a group that briefly shows an account who will
 * be refused is a lie. See `lib/developer-access.tsx` for why the sidebar asks at all.
 */
function visible(item: NavItem, canOpen: boolean | null): boolean {
  if (!("needsDeveloper" in item) || item.needsDeveloper !== true) {
    return true;
  }
  return canOpen === true;
}

/// Screens whose own path also prefixes their children (`/settings/iam` against
/// `/settings/iam/users`): the parent highlights only when it is exactly the open screen.
const EXACT_MATCH_ONLY = new Set<string>(["/settings/iam"]);

/** `true` when a navigation entry belongs to the screen that is open. */
function isActive(href: string, pathname: string): boolean {
  if (href === "/") {
    return pathname === "/";
  }
  if (EXACT_MATCH_ONLY.has(href)) {
    return pathname === href;
  }
  return pathname.startsWith(href);
}

function initials(value: string): string {
  const parts = value
    .trim()
    .split(/[\s@._-]+/)
    .filter(Boolean);
  if (parts.length === 0) {
    return "?";
  }
  if (parts.length === 1) {
    return parts[0].slice(0, 2).toUpperCase();
  }
  return (parts[0][0] + parts[1][0]).toUpperCase();
}

type AppShellProps = {
  /** Title of the current screen. */
  title: string;
  /** One line under the title. */
  description?: string;
  /** The screen. */
  children: ReactNode;
};

/** Frame every signed-in screen is rendered into. */
export function AppShell({ title, description, children }: AppShellProps) {
  const pathname = usePathname();
  const router = useRouter();
  const { user, signOut } = useSession();
  const { canOpen: canOpenDeveloper } = useDeveloperAccess();
  const [navOpen, setNavOpen] = useState(false);

  const handleSignOut = async () => {
    await signOut();
    router.replace("/login");
  };

  const sidebar = (
    <div className="flex h-full flex-col gap-6 p-4">
      <Link href="/" className="flex items-center gap-2.5 px-2 py-1" onClick={() => setNavOpen(false)}>
        <span
          aria-hidden
          className="flex size-8 items-center justify-center rounded-lg bg-accent text-sm font-semibold text-white"
        >
          O
        </span>
        <span className="flex flex-col leading-tight">
          <span className="text-[13.5px] font-semibold">Omnion</span>
          <span className="text-[11px] text-muted">Admin panel</span>
        </span>
      </Link>

      <nav aria-label="Sections" className="flex flex-col gap-1">
        {NAV.filter((item) => visible(item, canOpenDeveloper)).map((item) => {
          const active = isActive(item.href, pathname);
          const Icon = item.icon;
          return (
            <Link
              key={item.href}
              href={item.href}
              aria-current={active ? "page" : undefined}
              onClick={() => setNavOpen(false)}
              className={`flex items-center gap-2.5 rounded-lg px-2.5 py-2 text-[13px] transition ${
                active
                  ? "bg-accent-soft font-medium text-accent-strong"
                  : "text-muted hover:bg-quiet-soft hover:text-ink"
              }`}
            >
              <Icon className="size-4" aria-hidden />
              {item.label}
            </Link>
          );
        })}
      </nav>

      <div className="mt-auto flex flex-col gap-3 border-t border-line pt-4">
        {user ? (
          <div className="flex items-center gap-2.5 px-2">
            <span className="flex size-8 shrink-0 items-center justify-center rounded-full bg-quiet-soft text-[11px] font-semibold">
              {initials(user.display_name || user.email)}
            </span>
            <span className="flex min-w-0 flex-col leading-tight">
              <span className="truncate text-[12.5px] font-medium">
                {user.display_name || user.email}
              </span>
              <span className="truncate text-[11px] text-muted">{user.email}</span>
            </span>
          </div>
        ) : null}
        <button
          type="button"
          onClick={handleSignOut}
          className="flex items-center gap-2.5 rounded-lg px-2.5 py-2 text-left text-[13px] text-muted transition hover:bg-quiet-soft hover:text-ink"
        >
          <LogOut className="size-4" aria-hidden />
          Sign out
        </button>
      </div>
    </div>
  );

  return (
    <div className="flex min-h-screen bg-canvas">
      <aside className="hidden w-64 shrink-0 border-r border-line bg-surface lg:block">
        <div className="sticky top-0 h-screen">{sidebar}</div>
      </aside>

      {navOpen ? (
        <div className="fixed inset-0 z-40 lg:hidden">
          <button
            type="button"
            aria-label="Close navigation"
            onClick={() => setNavOpen(false)}
            className="absolute inset-0 bg-ink/40"
          />
          <div className="absolute inset-y-0 left-0 w-64 border-r border-line bg-surface shadow-xl">
            <button
              type="button"
              aria-label="Close navigation"
              onClick={() => setNavOpen(false)}
              className="absolute top-3 right-3 rounded-lg p-1.5 text-muted hover:bg-quiet-soft hover:text-ink"
            >
              <X className="size-4" aria-hidden />
            </button>
            {sidebar}
          </div>
        </div>
      ) : null}

      <div className="flex min-w-0 flex-1 flex-col">
        <header className="sticky top-0 z-30 border-b border-line bg-canvas/85 backdrop-blur">
          <div className="flex flex-wrap items-center gap-3 px-4 py-3 sm:px-6">
            <button
              type="button"
              aria-label="Open navigation"
              onClick={() => setNavOpen(true)}
              className="rounded-lg border border-line bg-surface p-2 text-muted transition hover:text-ink lg:hidden"
            >
              <Menu className="size-4" aria-hidden />
            </button>
            <div className="min-w-0 flex-1">
              <h1 className="truncate text-[15px] font-semibold">{title}</h1>
              {description ? (
                // From `sm` up the shell says what the screen is; on a phone there is no room for
                // it, and a sentence cut off mid-word reads as a defect rather than as a caption.
                <p className="hidden truncate text-[12px] text-muted sm:block">{description}</p>
              ) : null}
            </div>
            {/* The one search box: beside the site switcher on large screens, its own full-width
                row under the header on small ones. */}
            <GlobalSearch title={title} className="order-last w-full lg:order-none lg:w-80" />
            <NotificationBell />
            <SiteSwitcher />
          </div>
        </header>
        <main className="flex-1 px-4 py-6 sm:px-6">{children}</main>
      </div>
    </div>
  );
}
