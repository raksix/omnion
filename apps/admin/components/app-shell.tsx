"use client";

/**
 * The panel's frame: a sidebar with the sections, a sticky header with the current screen and
 * the site switcher, and the screen itself. Below `lg` the sidebar becomes a drawer.
 */
import { useState, type ReactNode } from "react";

import { Activity, BarChart3, Bell, BookMarked, Bot, ClipboardCheck, Cpu, FileStack, FileText, Fingerprint, Globe, Grid3x3, HardDriveDownload, HeartPulse, History as HistoryIcon, Images, Import, KeyRound, LayoutDashboard, LockKeyhole, LogOut, Menu, Scale, ScrollText, ShieldCheck, SlidersHorizontal, Sparkles, Timer, UserCog, UsersRound, Webhook, Wrench, X } from "lucide-react";
import Link from "next/link";
import { usePathname, useRouter } from "next/navigation";

import { SiteSwitcher } from "@/components/site-switcher";
import { GlobalSearch } from "@/components/global-search";
import { NotificationBell } from "@/components/notification-bell";
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
  // The agent runtime (REQ-099, slice 1). Two entries rather than one, because the two screens
  // answer two different questions: "what may I let this do" (the configuration) and "what did
  // it already do, and what did that cost" (the history). An operator reads them in that order
  // and very rarely in the other one.
  { href: "/ai/agents", label: "Agents", icon: Bot },
  // The skills registry (REQ-099, slice 3) is its own entry because it is a *library* rather
  // than a runtime screen: an operator maintains the guidance here and attaches it over there.
  { href: "/ai/skills", label: "Skills", icon: BookMarked },
  // The tool registry, the identities that grant them, and the matrix that shows both
  // (REQ-100). Three entries because they answer three different questions in the order an
  // operator asks them: what exists → what this organization decided → who ends up able to use
  // it. The registry entry was missing from the nav entirely until now, which is the sort of
  // omission that makes a finished feature look unfinished.
  { href: "/ai/tools", label: "Tool registry", icon: Wrench },
  { href: "/ai/identities", label: "AI identities", icon: ShieldCheck },
  { href: "/ai/permissions", label: "AI permissions", icon: Grid3x3 },
  // The review inbox (REQ-101). It sits right after the permission matrix because it answers the
  // question the matrix raises: knowing who may act still leaves "what is waiting for them" — and
  // an approval gate with no inbox is a gate nobody ever opens.
  { href: "/ai/approvals", label: "AI approvals", icon: ClipboardCheck },
  // The proposed operation lists (REQ-101 slice 3). Beside the inbox rather than under it: the
  // inbox decides one frozen call, a change set is a list a person edits first, and routing
  // "my agent proposed something" to a screen that can only reject it is a dead end.
  { href: "/ai/change-sets", label: "Change sets", icon: FileStack },
  { href: "/ai/runs", label: "Agent runs", icon: HistoryIcon },
  // The data guard (REQ-105). Two entries, not one: the policy is a *configuration* an operator
  // sets once and then forgets, while the event log is the thing they open when a call came back
  // refused and they need to know why. Routing both into one screen would mean the log — the only
  // reason an operator goes to the guard in the middle of an incident — sits behind a settings
  // page. The rules table is reachable from the policy panel's own rows.
  { href: "/ai/guard", label: "Data guard", icon: ShieldCheck },
  { href: "/ai/guard/events", label: "Guard events", icon: ScrollText },
  // Local inference (REQ-106). Beside the guard rather than under settings/iam because it is an
  // AI-Hub screen answering the same question the guard does from the other side: the guard asks
  // "what did the last call contain", this asks "where can a call go at all". The models table is
  // reachable from the endpoint row rather than from the sidebar, because a screen reached from a
  // specific endpoint is about that endpoint — listing it beside configuration invites an
  // operator to pull a model with no endpoint chosen.
  { href: "/ai/local", label: "Local AI", icon: Cpu },
  // The air-gap switch (REQ-106). Under "Local AI" rather than in the settings block, because it is
  // the second half of the same question — the endpoint list says where a call can still go, this
  // says what happens to the ones that cannot. It is the screen an operator opens mid-incident, so
  // burying it in /settings would put the control furthest from the incident.
  { href: "/ai/settings/airgap", label: "Air gap", icon: LockKeyhole },
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
] as const;

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
        {NAV.map((item) => {
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
