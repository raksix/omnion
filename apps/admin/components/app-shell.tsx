"use client";

/**
 * The panel's frame: a sidebar with the sections, a sticky header with the current screen and
 * the site switcher, and the screen itself. Below `lg` the sidebar becomes a drawer.
 */
import { useState, type ReactNode } from "react";

import { FileText, Globe, LayoutDashboard, LogOut, Menu, X } from "lucide-react";
import Link from "next/link";
import { usePathname, useRouter } from "next/navigation";

import { SiteSwitcher } from "@/components/site-switcher";
import { useSession } from "@/lib/session";

const NAV = [
  { href: "/", label: "Overview", icon: LayoutDashboard },
  { href: "/pages", label: "Pages", icon: FileText },
  { href: "/sites", label: "Sites", icon: Globe },
] as const;

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
          const active = item.href === "/" ? pathname === "/" : pathname.startsWith(item.href);
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
          <div className="flex items-center gap-3 px-4 py-3 sm:px-6">
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
                <p className="truncate text-[12px] text-muted">{description}</p>
              ) : null}
            </div>
            <SiteSwitcher />
          </div>
        </header>
        <main className="flex-1 px-4 py-6 sm:px-6">{children}</main>
      </div>
    </div>
  );
}
