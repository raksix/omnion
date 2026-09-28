"use client";

/**
 * The organization switcher: which tenant the panel is working in (REQ-005, slice 1).
 *
 * It sits beside the site switcher in the header, lists the caller's own memberships with the
 * roles they hold there, and switches by asking the API to move the session's home — the panel
 * then reloads, because every screen's data is tenant-scoped and a screen the caller may not
 * see in the new organization has to land on that organization's overview.
 *
 * Keyboard: `⌘⇧O` (or `Ctrl+Shift+O`) opens it, `↑`/`↓` move, `Enter` switches, `Esc` closes.
 */
import { useCallback, useEffect, useRef, useState } from "react";

import { Building2, Check, ChevronDown, Search, X } from "lucide-react";
import { usePathname, useRouter } from "next/navigation";

import {
  ApiError,
  fetchMyOrganizations,
  switchOrganization,
  type AccountOrganization,
} from "@/lib/api";
import { useSession } from "@/lib/session";
import { useTenantStatus } from "@/lib/tenant-status";

/** Read the list once per mount and after every switch. */
export function OrganizationSwitcher() {
  const { user, status: sessionStatus } = useSession();
  const { reload: reloadTenantStatus } = useTenantStatus();
  const router = useRouter();
  const pathname = usePathname();
  const [organizations, setOrganizations] = useState<AccountOrganization[]>([]);
  const [currentId, setCurrentId] = useState<string | null>(null);
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const rootRef = useRef<HTMLDivElement | null>(null);

  const load = useCallback(async () => {
    try {
      const body = await fetchMyOrganizations();
      setOrganizations(body.organizations);
      setCurrentId(body.current_organization_id);
    } catch (cause) {
      setError(
        cause instanceof ApiError ? cause.message : "The organizations could not be loaded.",
      );
    }
  }, []);

  useEffect(() => {
    if (sessionStatus !== "signed-in") return;
    void load();
  }, [sessionStatus, load]);

  // A click outside closes the dropdown; the panel's own overlay does not cover the header.
  useEffect(() => {
    if (!open) return;
    const onPointerDown = (event: MouseEvent) => {
      if (!rootRef.current?.contains(event.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", onPointerDown);
    return () => document.removeEventListener("mousedown", onPointerDown);
  }, [open]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape" && open) {
        setOpen(false);
        return;
      }
      if (event.key.toLowerCase() === "o" && (event.metaKey || event.ctrlKey) && event.shiftKey) {
        event.preventDefault();
        setOpen((value) => !value);
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [open]);

  const current = organizations.find((organization) => organization.organization_id === currentId);
  const visible = organizations.filter((organization) => {
    const needle = query.trim().toLowerCase();
    if (!needle) return true;
    return organization.name.toLowerCase().includes(needle);
  });

  const choose = async (organization: AccountOrganization) => {
    if (organization.organization_id === currentId) {
      setOpen(false);
      return;
    }
    setBusyId(organization.organization_id);
    setError(null);
    try {
      await switchOrganization(organization.organization_id);
      setOpen(false);
      setCurrentId(organization.organization_id);
      // A screen the caller may not see in the new organization redirects itself, so the panel
      // only has to reload; the query is cleared so a stale filter does not leak across.
      router.replace(pathname);
      router.refresh();
      await load();
      // The freeze banner reads the *current* tenant, so a switch into a suspended one has to
      // reach it. Without this the banner keeps reporting the tenant just left, which is worse
      // than showing nothing: the panel looks writable while the API refuses every write.
      reloadTenantStatus();
    } catch (cause) {
      setError(
        cause instanceof ApiError ? cause.message : "The organization could not be switched.",
      );
    } finally {
      setBusyId(null);
    }
  };

  const onListKeyDown = (event: React.KeyboardEvent) => {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      setActive((index) => Math.min(index + 1, visible.length - 1));
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      setActive((index) => Math.max(index - 1, 0));
    } else if (event.key === "Enter") {
      event.preventDefault();
      const target = visible[active];
      if (target) void choose(target);
    }
  };

  // A platform account (no membership) has nothing to switch between, and a single-tenant
  // account would be clicking at a control with one row: neither gets the switcher.
  if (sessionStatus !== "signed-in" || organizations.length === 0) {
    if (organizations.length === 0 && !error && user?.organization_id === null) {
      return (
        <span
          className="hidden items-center gap-2 rounded-lg border border-dashed border-line px-2.5 py-2 text-[12px] text-muted sm:flex"
        >
          <Building2 className="size-3.5" aria-hidden />
          No organization
        </span>
      );
    }
    return null;
  }

  return (
    <div ref={rootRef} className="relative">
      <button
        type="button"
        onClick={() => {
          setOpen((value) => !value);
          setActive(0);
          setQuery("");
        }}
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-label="Current organization"
        className="flex max-w-52 items-center gap-2 rounded-lg border border-line bg-surface px-2.5 py-1.5"
      >
        <Building2 className="size-3.5 shrink-0 text-muted" aria-hidden />
        <span className="min-w-0 flex-1 truncate text-left text-[12.5px] font-medium">
          {current?.name ?? "Organization"}
        </span>
        <ChevronDown className="size-3.5 shrink-0 text-muted" aria-hidden />
      </button>

      {open ? (
        <>
          {/* A phone has no room for a floating panel beside the header, and a dropdown that
              hangs off the right edge of a 390px screen puts the longest organization name in
              the one place a reader cannot scroll to. Under `sm` the switcher is therefore a
              bottom sheet that covers the screen, with 44px rows and a close control; from `sm`
              up it is the panel that drops out of the button. Same state, same keyboard, two
              shapes — the layout is the only thing that changes. */}
          <button
            type="button"
            aria-label="Close the organization switcher"
            onClick={() => setOpen(false)}
            className="fixed inset-0 z-40 bg-ink/40 sm:hidden"
          />
          <div
            role="dialog"
            aria-label="Switch organization"
            data-org-switcher="sheet"
            className="fixed inset-x-0 bottom-0 z-50 flex max-h-[85vh] flex-col overflow-hidden rounded-t-2xl border-t border-line bg-surface shadow-2xl sm:absolute sm:inset-x-auto sm:bottom-auto sm:right-0 sm:mt-1.5 sm:max-h-none sm:w-72 sm:rounded-xl sm:border sm:shadow-xl"
          >
          <div className="flex items-center gap-1.5 border-b border-line px-3 py-2">
            <Search className="size-3.5 text-muted" aria-hidden />
            <span className="sr-only">Search organizations</span>
            <input
              value={query}
              onChange={(event) => {
                setQuery(event.target.value);
                setActive(0);
              }}
              placeholder="Search organizations"
              className="w-full bg-transparent text-[12.5px] outline-none"
            />
            <button
              type="button"
              onClick={() => setOpen(false)}
              aria-label="Close"
              className="shrink-0 rounded-md p-1.5 text-muted transition hover:bg-quiet-soft hover:text-ink sm:hidden"
            >
              <X className="size-4" aria-hidden />
            </button>
          </div>

          <ul
            role="listbox"
            aria-label="Your organizations"
            className="min-h-0 flex-1 overflow-y-auto py-1 sm:max-h-72"
          >
            {visible.map((organization, index) => {
              const selected = organization.organization_id === currentId;
              return (
                <li key={organization.organization_id}>
                  <button
                    type="button"
                    role="option"
                    aria-selected={selected}
                    onMouseEnter={() => setActive(index)}
                    onClick={() => void choose(organization)}
                    disabled={busyId === organization.organization_id}
                    // 44px on touch, 32px on a pointer: the row is the whole target and a
                    // two-line row is taller than both, so the floor is the *minimum* here.
                    className={`flex min-h-11 w-full items-center gap-2 px-3 py-2 text-left transition sm:min-h-0 ${
                      index === active ? "bg-quiet-soft" : ""
                    }`}
                  >
                    <span className="min-w-0 flex-1">
                      <span className="flex min-w-0 items-center gap-1.5">
                        <span className="truncate text-[13px] font-medium">
                          {organization.name}
                        </span>
                        {organization.organization_status !== "active" ? (
                          <span className="rounded-full bg-caution-soft px-1.5 py-0.5 text-[10.5px] font-medium text-caution">
                            {organization.organization_status}
                          </span>
                        ) : null}
                      </span>
                      <span className="mt-0.5 flex flex-wrap gap-1">
                        {organization.roles.length === 0 ? (
                          <span className="text-[11px] text-muted">No roles</span>
                        ) : (
                          organization.roles.map((role) => (
                            <span
                              key={role.key}
                              className="max-w-32 truncate rounded-md border border-line px-1.5 py-0.5 text-[10.5px] text-muted"
                            >
                              {role.name}
                            </span>
                          ))
                        )}
                      </span>
                    </span>
                    {selected ? (
                      <Check className="size-3.5 shrink-0 text-accent-strong" aria-hidden />
                    ) : null}
                  </button>
                </li>
              );
            })}
          </ul>

          <div className="border-t border-line px-3 py-2 text-[11.5px] text-muted">
            <span onKeyDown={onListKeyDown} tabIndex={-1} className="block outline-none">
              Switch with ↑ ↓ and Enter · <kbd>⌘⇧O</kbd> toggles
            </span>
          </div>

          {error ? (
            <p role="alert" className="border-t border-line px-3 py-2 text-[12px] text-accent-strong">
              {error}
            </p>
          ) : null}
          </div>
        </>
      ) : null}
    </div>
  );
}
