"use client";

/**
 * The panel's frame: a sidebar with the sections, a sticky header with the current screen and
 * the site switcher, and the screen itself. Below `lg` the sidebar becomes a drawer.
 */
import { useEffect, useMemo, useState, type ReactNode } from "react";

import {
  Activity,
  AppWindow,
  BarChart3,
  Bell,
  Bot,
  Building2,
  ClipboardCheck,
  Compass,
  FileText,
  Fingerprint,
  Gauge,
  Globe,
  HardDriveDownload,
  HeartPulse,
  Images,
  Import,
  KeyRound,
  Layers,
  LayoutDashboard,
  LockKeyhole,
  LogOut,
  Menu,
  Rocket,
  Radio,
  Scale,
  ScrollText,
  ShieldCheck,
  SlidersHorizontal,
  Sparkles,
  Timer,
  UserCog,
  UsersRound,
  Webhook,
  X,
  type LucideIcon,
} from "lucide-react";
import Link from "next/link";
import { usePathname, useRouter } from "next/navigation";

import { SiteSwitcher } from "@/components/site-switcher";
import { OrganizationSwitcher } from "@/components/organization-switcher";
import { GlobalSearch } from "@/components/global-search";
import { NotificationBell } from "@/components/notification-bell";
import { EnvironmentChip, StagingEnvironmentBanner } from "@/components/environment-chip";
import { MaintenanceWindowBanner } from "@/components/maintenance-window-banner";
import { TenantStatusBanner } from "@/components/tenant-status-banner";
import { useSession } from "@/lib/session";
import { useTenantStatus } from "@/lib/tenant-status";

/** One sidebar entry. `module` is the switch that hides it (REQ-005, slice 4). */
type NavItem = {
  href: string;
  label: string;
  icon: LucideIcon;
  module?: string;
};

const NAV: readonly NavItem[] = [
  { href: "/", label: "Overview", icon: LayoutDashboard },
  { href: "/pages", label: "Pages", icon: FileText },
{ href: "/media", label: "Media", icon: Images, module: "media" },
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
  { href: "/analytics", label: "Analytics", icon: BarChart3, module: "analytics" },
  { href: "/notifications", label: "Notifications", icon: Bell },
  // Exact-match only: `/cdn` is the overview and `/cdn/rules` is a different screen, so the
  // overview would otherwise light up for the whole section and the active entry would be
  // whichever the reader happened to be furthest from.
  { href: "/cdn", label: "CDN", icon: Gauge },
  // The event console (REQ-016, slice 1). It sits beside Notifications rather than under
  // Settings because both answer the same question from the bus's side — "what does the
  // platform think happened" and "who was told" — and an operator chasing a missing webhook
  // needs both on the same shelf.
  { href: "/events", label: "Events", icon: Activity },
  // Staging environments (REQ-017, slice 2). Beside Sites rather than under Settings: a staging
  // copy of a site's content is a fact about the site, and an operator who is editing pages wants
  // "which copy am I editing" one click from the pages, not four levels down.
  { href: "/environments", label: "Environments", icon: Layers },
  // The deployment centre (REQ-024). Beside Environments rather than under Settings, and for
  // the same reason: "which version is running, and may it be upgraded" is the question an
  // operator opens the panel to answer, and burying it four levels down is how a deployment
  // screen becomes the thing nobody looks at.
  { href: "/deployment", label: "Deployment", icon: Rocket },
  // The endpoints and their delivery history (REQ-016, slice 2). It sits next to Events
  // rather than under Settings because the two are the same investigation from both ends:
  // the feed says what happened, this says who was told and whether they got it.
  { href: "/webhooks", label: "Webhooks", icon: Webhook },
  // The developer platform (REQ-033, slice 1). Beside Webhooks rather than under Settings: a
  // developer asking "what can I call, and what has my integration already called" is asking
  // about the same platform surface from two ends, and both answers belong on one shelf.
  // `/developer` is the section root the REQ names; the keys and logs screens ship with it and
  // the overview arrives with slice 4, so the root is registered once it exists rather than
  // pointing at a screen that has not been built.
  // The Explorer (REQ-033, slice 2) sits *before* the keys: a developer who opens the
  // developer section is usually trying to make a call, and the reference is the first
  // question, not the third. The key screen is where you answer "how do I authenticate"
  // once the Explorer has told you what the call is.
  { href: "/developer/api-explorer", label: "Developer · API Explorer", icon: Compass },
  { href: "/developer/keys", label: "Developer · API keys", icon: KeyRound },
  // The OAuth app registry (REQ-033, slice 3) sits *after* the keys and not before them: a
  // developer arrives here with one of two questions — "how do I authenticate my own server?"
  // (keys) or "let somebody else sign in" (an app) — and only the second one needs a registry.
  { href: "/developer/oauth-apps", label: "Developer · OAuth apps", icon: AppWindow },
  // The catalogue follows the app registry and not /events, because the question changes: the
  // registry answers "let somebody else sign in", and the catalogue answers "what will arrive
  // when they do" — which is the next thing a developer building a subscriber needs to read.
  // The same registry is on /events for an operator watching the feed; this is the contract.
  { href: "/developer/events", label: "Developer · Event catalogue", icon: Radio },
  { href: "/developer/logs", label: "Developer · Logs", icon: ScrollText },
  { href: "/sites", label: "Sites", icon: Globe },
  { href: "/organizations", label: "Organizations", icon: Building2 },
  { href: "/ai", label: "AI Hub", icon: Sparkles, module: "ai-hub" },
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
];

/// Screens whose own path also prefixes their children (`/settings/iam` against
/// `/settings/iam/users`): the parent highlights only when it is exactly the open screen.
const EXACT_MATCH_ONLY = new Set<string>(["/settings/iam", "/cdn"]);

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
  const { disabledModules, modulesLoaded } = useTenantStatus();
  const [navOpen, setNavOpen] = useState(false);

  // A module switched off for this tenant takes its sidebar entry with it. The API refuses the
  // module's routes with `403 organization.module.disabled` (REQ-005, slice 4), so leaving the
  // link would be a menu entry that cannot be followed — the REQ's acceptance line asks for the
  // entry to *hide*, and a hidden entry is the only version of this that is not a bug report.
  //
  // `modulesLoaded` is the load-bearing part: before the answer arrives the list is complete,
  // because a sidebar that empties itself while a request is in flight and refills afterwards
  // flickers, and a flicker in a menu reads as a broken panel rather than a pending one.
  const hiddenModules = useMemo(
    () => (modulesLoaded ? new Set(disabledModules) : new Set<string>()),
    [disabledModules, modulesLoaded],
  );
  const navItems = useMemo(
    () => NAV.filter((item) => !item.module || !hiddenModules.has(item.module)),
    [hiddenModules],
  );

  // A person who was already inside a module when it was switched off is left on a screen whose
  // menu entry no longer exists. Sending them to the overview is the honest answer: the screen
  // they are on answers `403`, and a panel that shows a dead screen is worse than one that
  // explains where they are.
  const inHiddenModule = useMemo(
    () =>
      NAV.some(
        (item) => item.module && hiddenModules.has(item.module) && isActive(item.href, pathname),
      ),
    [hiddenModules, pathname],
  );
  useEffect(() => {
    if (inHiddenModule) router.replace("/");
  }, [inHiddenModule, router]);

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

      {/* The list scrolls; the brand above it and the account block below it do not.
        *
        * This frame is `sticky top-0 h-screen`, so it is exactly one viewport tall and, before
        * this, its contents were not either: `h-full` on a flex column with no `overflow` clips
        * what does not fit and gives no way to reach it. With ~34 entries the identity/access
        * shelves put the last links ("Sessions", "Devices", "Search settings") roughly 400px below
        * a 900px fold — permanently unreachable, with no scrollbar to say so. A QA pass reported
        * it as three `click-error`s on those exact links while every one of them looked perfectly
        * normal in a screenshot, because "rendered" and "reachable" are different properties and
        * only a click measures the second.
        *
        * `min-h-0` is load-bearing: a flex child defaults to `min-height: auto`, so it refuses to
        * shrink below its content and `overflow-y-auto` would never engage. `overscroll-contain`
        * keeps the wheel inside the list instead of chaining to the page once it reaches the end,
        * which is what makes the end of the list feel like a wall rather than a dead zone.
        */}
      <nav aria-label="Sections" className="min-h-0 flex-1 overflow-y-auto overscroll-contain">
        <div className="flex flex-col gap-1 pr-1">
        {navItems.map((item) => {
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
        </div>
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
          {/* The freeze notice is *inside* the sticky header rather than beside it: a banner that
              scrolls away is a banner a person reads once and then forgets, and the whole point
              is that it stays until the tenant is reactivated. */}
          <TenantStatusBanner />
          {/* Staging sits directly under the freeze notice and above the title row, for the same
              reason: a banner that scrolls away is a banner a person reads once and forgets. */}
          <StagingEnvironmentBanner />
          {/* The maintenance window (REQ-024, slice 3) sits below both, and inside this sticky
              header for the same reason with a sharper edge: it is the notice that the operator's
              next Save is about to be refused, so it has to be in front of them at the moment
              they press it. No dismiss control, for the reason the staging strip has none -- an
              operator who may hide "your writes are being refused" will hide it. */}
          <MaintenanceWindowBanner />
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
            {/* The environment chip sits with the other tenant-scoped chrome, before the site
                switcher: which copy of the content you are in is a *wider* fact than which site,
                and a person reading left to right meets it first. */}
            <EnvironmentChip />
            <OrganizationSwitcher />
            <SiteSwitcher />
          </div>
        </header>
        <main className="flex-1 px-4 py-6 sm:px-6">{children}</main>
      </div>
    </div>
  );
}
