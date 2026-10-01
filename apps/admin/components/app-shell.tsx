"use client";

/**
 * The panel's frame: a sidebar with the sections, a sticky header with the current screen and
 * the site switcher, and the screen itself. Below `lg` the sidebar becomes a drawer.
 */
import { useState, type ReactNode } from "react";

import {
  Activity,
  BarChart3,
  Bell,
  Bot,
  Boxes,
  CalendarClock,
  ClipboardCheck,
  FileStack,
  FileText,
  Fingerprint,
  Globe,
  HardDriveDownload,
  HeartPulse,
  Images,
  Import,
  KeyRound,
  LayoutDashboard,
  LayoutGrid,
  ListTree,
  LockKeyhole,
  LogOut,
  Mail,
  Menu,
  MessageSquare,
  Palette,
  Scale,
  ScrollText,
  ShieldCheck,
  SlidersHorizontal,
  Sparkles,
  Timer,
  UserCog,
  UserRoundCheck,
  UsersRound,
  Webhook,
  X,
} from "lucide-react";
import Link from "next/link";
import { usePathname, useRouter } from "next/navigation";

import { SiteSwitcher } from "@/components/site-switcher";
import { GlobalSearch } from "@/components/global-search";
import { NotificationBell } from "@/components/notification-bell";
import { useSession } from "@/lib/session";

const NAV = [
  { href: "/", label: "Overview", icon: LayoutDashboard },
  { href: "/pages", label: "Pages", icon: FileText },
  { href: "/blocks", label: "Blocks", icon: Boxes },
  { href: "/patterns", label: "Patterns", icon: LayoutGrid },
  { href: "/page-templates", label: "Page templates", icon: FileStack },
  // The navigation editor and the queue of scheduled publishes (REQ-064, slice 1). Both are
  // content surfaces rather than settings, so they sit next to Pages rather than under it.
  { href: "/menus", label: "Menus", icon: ListTree },
  { href: "/publishing/queue", label: "Publishing queue", icon: CalendarClock },
  // The SEO toolkit (REQ-064, slice 3). It sits beside Publishing queue because both answer
  // "what does the outside world see about this site" — one about when, one about how.
  { href: "/seo", label: "SEO", icon: Globe },
  // The comment queue (REQ-064, slice 4a). It sits beside SEO rather than under Pages because
  // it is the one content surface that is *inbound*: everything else in this shelf is
  // something an editor wrote, and the queue is the only place an owner finds out what
  // readers said about it.
  { href: "/comments", label: "Comments", icon: MessageSquare },
  // The mailing lists (REQ-064, slice 4b). It sits directly after Comments because both answer
  // the same inbound question — what visitors sent us — and both are the places an owner has to
  // decide about a stranger's address. Everything before them in this shelf is an outbound or
  // authored surface.
  { href: "/newsletter", label: "Newsletter", icon: Mail },
  // Visitor accounts (REQ-064, slice 4c). It sits directly after Newsletter because both hold
  // a stranger's address and both ask an operator to decide about one — and the icon differs on
  // purpose: `UserCog` already means a PANEL user two rows down, and an operator who confuses
  // the two is about to grant a visitor a set of platform permissions.
  { href: "/members", label: "Members", icon: UserRoundCheck },
  // The headless content API (REQ-019, slice 1). It sits AFTER Members rather than beside
  // Settings because the thing it manages is a credential that leaves the building: an operator
  // asking "who outside our org is reading our site" is the same question as "who is a member",
  // asked about a stranger rather than an account. `KeyRound` rather than a settings key, because
  // it is a credential and not a preference.
  { href: "/content-api", label: "Content API", icon: KeyRound },
  // The theme gallery (REQ-062). It sits directly BEFORE Media on purpose: the gallery decides
  // what a visitor sees, and every row below it in this shelf — media, menus, pages — is content
  // a theme then presents. `Palette` rather than `LayoutTemplate`, which the block editor
  // already owns one screen over.
  { href: "/themes", label: "Themes", icon: Palette },
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
