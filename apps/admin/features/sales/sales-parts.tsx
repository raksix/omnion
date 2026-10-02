"use client";

/**
 * The frame the `/sales` screens share (REQ-052, slice 1).
 *
 * One second-level tab bar under the app shell, and the keyboard contract the spec asks for: `/`
 * reaches the search, `j`/`k` move the selected row, `Enter` opens it, `e` edits it, `n` starts a
 * new record and `?` shows the shortcut sheet. The shortcuts are ignored while a field has focus —
 * a form is not a keyboard surface for the list behind it.
 *
 * The list's state lives in the **URL**, for the same reason the CRM lists put it there: a filtered
 * list is a link, so the back button, a bookmark and a shared URL all land on the same rows.
 *
 * The tab bar lists only what the shipped slices provide. `Reports` is still absent rather than
 * disabled, because a tab that leads nowhere is a dead button, and the request describes it rather
 * than the code.
 */
import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";

import { Keyboard } from "lucide-react";
import Link from "next/link";
import { usePathname, useRouter, useSearchParams } from "next/navigation";

import { fetchMe } from "@/lib/api";
import { fetchSalesVocabulary, type SalesVocabulary } from "@/lib/sales";

/**
 * The screens the shipped slices provide. The rest arrive with their own slices, not as dead tabs.
 *
 * `Quotes` is first because it is where a seller spends the day, and `Orders` second because it is
 * where a quote ends up; the catalog and the price lists are the two screens both draw their lines
 * from, and the settings row is what every one of them reads. `Reports` is still absent on
 * purpose: a nav entry that leads nowhere is worse than no entry, and it arrives with the rest of
 * slice 4.
 */
export const SALES_NAV = [
  { href: "/sales/quotes", label: "Quotes", shortcut: "q" },
  { href: "/sales/orders", label: "Orders", shortcut: "o" },
  { href: "/sales/approvals", label: "Approvals", shortcut: "a" },
  { href: "/sales/catalog", label: "Catalog", shortcut: "c" },
  { href: "/sales/pricelists", label: "Price lists", shortcut: "p" },
  { href: "/sales/reports", label: "Reports", shortcut: "r" },
  { href: "/sales/settings", label: "Settings", shortcut: "s" },
] as const;

/** One shortcut, as the sheet prints it. */
type Shortcut = { keys: string; what: string };

/** The sheet, identical across the section so it is learned once. */
const SHORTCUTS: Shortcut[] = [
  { keys: "/", what: "Focus the search" },
  { keys: "j / k", what: "Move the selected row" },
  { keys: "Enter", what: "Open the selected row" },
  { keys: "e", what: "Edit the selected row" },
  { keys: "n", what: "New record" },
  { keys: "g then q", what: "Go to the quotes" },
  { keys: "g then o", what: "Go to the orders" },
  { keys: "g then a", what: "Go to the approvals" },
  { keys: "g then c", what: "Go to the catalog" },
  { keys: "g then p", what: "Go to the price lists" },
  { keys: "g then r", what: "Go to the reports" },
  { keys: "?", what: "Show or hide this sheet" },
  { keys: "Esc", what: "Close the sheet, or leave the field" },
];

// ---------------------------------------------------------------------------------------------
// Tenant
// ---------------------------------------------------------------------------------------------

type SalesContextValue = {
  /** The organization the panel is reading, or null on a tenant-bound account. */
  organizationId: string | null;
  /** The categories and units the catalog form offers, loaded once for the section. */
  vocabulary: SalesVocabulary | null;
};

const SalesContext = createContext<SalesContextValue>({ organizationId: null, vocabulary: null });

/** The section-wide tenant and vocabulary, so two screens cannot disagree about either. */
export function useSales(): SalesContextValue {
  return useContext(SalesContext);
}

/**
 * Provides the tenant the sales screens read.
 *
 * It is a provider rather than a hook each screen calls because the decision is not a screen's
 * business: `/sales/catalog` and `/sales/pricelists` opened on the same account must not be able to
 * show two different tenants, and the API refuses a platform account on a multi-organization
 * installation that has not named one — advice that is impossible to follow if the name lives in
 * one screen only.
 */
export function SalesTenantProvider({ children }: { children: ReactNode }) {
  const [organizationId, setOrganizationId] = useState<string | null>(null);
  const [vocabulary, setVocabulary] = useState<SalesVocabulary | null>(null);

  // A tenant-bound account reads its own organization and the panel has nothing to choose. A
  // platform account has to name one, so the choice is a screen's business and lives in the URL.
  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const me = await fetchMe();
        if (cancelled) return;
        setOrganizationId(me.organization_id ?? null);
      } catch {
        // A session that cannot be read is handled by the shell's own guard; leaving the tenant
        // null here would only produce a second, less specific error on the screen.
        if (!cancelled) setOrganizationId(null);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const value = useMemo<SalesContextValue>(() => ({ organizationId, vocabulary }), [organizationId, vocabulary]);

  return (
    <SalesContext.Provider value={value}>
      <VocabularyLoader organizationId={organizationId} onLoaded={setVocabulary} />
      {children}
    </SalesContext.Provider>
  );
}

/** Loads the catalog vocabulary once the tenant is known. */
function VocabularyLoader({
  organizationId,
  onLoaded,
}: {
  organizationId: string | null;
  onLoaded: (value: SalesVocabulary) => void;
}) {
  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const loaded = await fetchSalesVocabulary(organizationId);
        if (!cancelled) onLoaded(loaded);
      } catch {
        // The vocabulary is a convenience (a dropdown's choices), not the screen's data: a failure
        // leaves the form with its own typed input rather than blocking the whole page.
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [organizationId, onLoaded]);

  return null;
}

// ---------------------------------------------------------------------------------------------
// The tab bar
// ---------------------------------------------------------------------------------------------

/** The section's tab bar, with the current screen marked for assistive technology. */
export function SalesTabs() {
  const pathname = usePathname();
  return (
    <nav aria-label="Sales sections" className="mb-4 flex flex-wrap items-center gap-1">
      {SALES_NAV.map((item) => {
        const current = pathname === item.href || pathname.startsWith(`${item.href}/`);
        return (
          <Link
            key={item.href}
            href={item.href}
            aria-current={current ? "page" : undefined}
            className={`rounded-md px-2.5 py-1.5 text-[13px] ${
              current ? "bg-quiet-soft font-medium" : "text-muted hover:text-ink"
            }`}
          >
            {item.label}
          </Link>
        );
      })}
    </nav>
  );
}

// ---------------------------------------------------------------------------------------------
// The keyboard contract
// ---------------------------------------------------------------------------------------------

/** The props a row needs to draw and read the shared cursor. */
export type SalesListKeyboard = {
  /** How many rows a `j`/`k` may reach. */
  count: number;
  /** The row the cursor is on. */
  selected: number;
  /** Move the cursor. */
  onSelect: (index: number) => void;
  /** Open the row under the cursor. */
  onOpen: (index: number) => void;
  /** Edit the row under the cursor. */
  onEdit?: (index: number) => void;
  /** Start a new record. */
  onNew?: () => void;
};

/** The attributes a row carries so the cursor is visible and the pass can find it. */
export function salesRowCursor(selected: boolean): {
  "data-qa-sales-cursor": string;
  "aria-selected"?: true;
  className: string;
} {
  return selected
    ? {
        "data-qa-sales-cursor": "true",
        "aria-selected": true,
        className: "bg-quiet-soft",
      }
    : { "data-qa-sales-cursor": "false", className: "" };
}

/** True when a keystroke belongs to a field rather than to the list. */
function isTyping(target: EventTarget | null): boolean {
  const element = target as HTMLElement | null;
  return (
    element instanceof HTMLInputElement ||
    element instanceof HTMLTextAreaElement ||
    element instanceof HTMLSelectElement ||
    element?.isContentEditable === true
  );
}

/**
 * The section's keyboard contract, and the sheet that documents it.
 *
 * A shortcut a person cannot see is indistinguishable from a broken one, so the sheet is reachable
 * with `?` and the cursor is drawn on the row it points at. `g`-prefixed navigation is the one
 * non-obvious part: it is a sequence rather than a single key because `c`, `p` and `s` are wanted
 * as "the key on the selected row" in some screens, and a bare letter that both navigates and acts
 * is a shortcut that surprises.
 */
export function useSalesKeyboard(
  keys: SalesListKeyboard,
  searchRef: React.RefObject<HTMLInputElement | null>,
): { shortcutsOpen: boolean; setShortcutsOpen: (open: boolean) => void } {
  const [shortcutsOpen, setShortcutsOpen] = useState(false);
  const pendingG = useRef(false);
  const router = useRouter();

  const { count, selected, onSelect, onOpen, onEdit, onNew } = keys;

  const move = useCallback(
    (delta: number) => {
      if (count === 0) return;
      onSelect(Math.min(Math.max(selected + delta, 0), count - 1));
    },
    [count, selected, onSelect],
  );

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.metaKey || event.ctrlKey || event.altKey) return;

      if (event.key === "Escape") {
        if (shortcutsOpen) {
          setShortcutsOpen(false);
          return;
        }
        if (document.activeElement === searchRef.current) {
          searchRef.current?.blur();
        }
        return;
      }

      if (event.key === "/" && !isTyping(event.target)) {
        event.preventDefault();
        searchRef.current?.focus();
        return;
      }

      if (event.key === "?" && !isTyping(event.target)) {
        event.preventDefault();
        setShortcutsOpen(!shortcutsOpen);
        return;
      }

      if (isTyping(event.target)) return;

      // `g` arms a jump; the window is short because the alternative is a letter that both
      // navigates and acts, and a person who typed `n` in the wrong place should not be teleported
      // a minute later.
      if (pendingG.current) {
        pendingG.current = false;
        // The four screens, matched by the single key each nav entry advertises. Written as a
        // lookup rather than four `if`s so adding a tab is one line here and cannot forget the
        // handler — which is how a nav entry ends up pointing at a screen nothing navigates to.
        const target = SALES_NAV.find((item) => item.shortcut === event.key);
        if (target) {
          event.preventDefault();
          router.push(target.href);
          return;
        }
      }
      if (event.key === "g") {
        pendingG.current = true;
        window.setTimeout(() => {
          pendingG.current = false;
        }, 1200);
        return;
      }

      if (event.key === "j" || event.key === "ArrowDown") {
        event.preventDefault();
        move(1);
        return;
      }
      if (event.key === "k" || event.key === "ArrowUp") {
        event.preventDefault();
        move(-1);
        return;
      }
      if (event.key === "Enter") {
        event.preventDefault();
        onOpen(selected);
        return;
      }
      if (event.key === "e" && onEdit) {
        event.preventDefault();
        onEdit(selected);
        return;
      }
      if (event.key === "n" && onNew) {
        event.preventDefault();
        onNew();
      }
    };

    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [count, selected, onSelect, onOpen, onEdit, onNew, move, router, searchRef, shortcutsOpen]);

  return { shortcutsOpen, setShortcutsOpen };
}

/** The sheet, opened with `?`. */
export function SalesShortcutSheet({ onClose }: { onClose: () => void }) {
  const ref = useRef<HTMLButtonElement | null>(null);
  useEffect(() => {
    ref.current?.focus();
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label="Keyboard shortcuts"
      data-qa-sales-shortcuts="open"
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/30 p-4"
      onClick={onClose}
    >
      <div
        className="w-full max-w-sm rounded-lg border border-line bg-panel p-4 shadow-lg"
        onClick={(event) => event.stopPropagation()}
      >
        <div className="mb-3 flex items-center justify-between">
          <p className="flex items-center gap-2 text-[13px] font-medium">
            <Keyboard className="h-4 w-4" aria-hidden />
            Keyboard shortcuts
          </p>
          <button
            ref={ref}
            type="button"
            onClick={onClose}
            className="rounded-md px-2 py-1 text-[12px] text-muted hover:text-ink"
          >
            Close
          </button>
        </div>
        <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1.5 text-[12.5px]">
          {SHORTCUTS.map((shortcut) => (
            <div key={shortcut.keys} className="contents">
              <dt className="font-mono text-[11.5px] text-muted">{shortcut.keys}</dt>
              <dd>{shortcut.what}</dd>
            </div>
          ))}
        </dl>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------------------------
// The toolbar
// ---------------------------------------------------------------------------------------------

/** The search field, a filter, and the row count every list shows. */
export function SalesToolbar({
  search,
  onSearchChange,
  searchRef,
  children,
  count,
  filtered,
}: {
  search: string;
  onSearchChange: (value: string) => void;
  searchRef: React.RefObject<HTMLInputElement | null>;
  /** The filters this particular list has. */
  children?: ReactNode;
  /** How many rows are on the page. */
  count: number;
  /** Whether any filter is narrowing the list, which the count then says so. */
  filtered: boolean;
}) {
  const params = useSearchParams();
  const router = useRouter();
  const pathname = usePathname();

  // Typing rewrites the URL rather than a piece of state, so a filtered list is a link — and the
  // debounce is what keeps it from being a history entry per keystroke.
  const push = useCallback(
    (next: URLSearchParams) => {
      const text = next.toString();
      router.replace(text ? `${pathname}?${text}` : pathname, { scroll: false });
    },
    [pathname, router],
  );

  const searchTimer = useRef<number | null>(null);
  const onSearch = (value: string) => {
    onSearchChange(value);
    if (searchTimer.current !== null) window.clearTimeout(searchTimer.current);
    searchTimer.current = window.setTimeout(() => {
      const next = new URLSearchParams(params.toString());
      if (value.trim() === "") next.delete("search");
      else next.set("search", value.trim());
      push(next);
    }, 250);
  };

  return (
    <div className="mb-3 flex flex-wrap items-center gap-2">
      <input
        ref={searchRef}
        type="search"
        value={search}
        onChange={(event) => onSearch(event.target.value)}
        placeholder="Search…"
        aria-label="Search"
        data-qa-sales-search
        className="min-w-48 flex-1 rounded-md border border-line bg-panel px-2.5 py-1.5 text-[13px] outline-none focus:border-ink-soft"
      />
      {children}
      <span className="text-[12px] text-muted" data-qa-sales-count>
        {filtered ? `${count} matching` : `${count} total`}
      </span>
    </div>
  );
}

/** A filter chip that is a real toggle in the URL, not a local box. */
export function SalesFilterChip({
  name,
  value,
  active,
  children,
}: {
  name: string;
  value: string;
  active: boolean;
  children: ReactNode;
}) {
  const params = useSearchParams();
  const router = useRouter();
  const pathname = usePathname();

  return (
    <button
      type="button"
      aria-pressed={active}
      data-qa-sales-filter={name}
      onClick={() => {
        const next = new URLSearchParams(params.toString());
        if (active) next.delete(name);
        else next.set(name, value);
        const text = next.toString();
        router.replace(text ? `${pathname}?${text}` : pathname, { scroll: false });
      }}
      className={`rounded-md px-2.5 py-1.5 text-[12.5px] ${
        active ? "bg-quiet-soft font-medium" : "text-muted hover:text-ink"
      }`}
    >
      {children}
    </button>
  );
}

/** A select that writes its value into the URL, for a filter with more than two states. */
export function SalesFilterSelect({
  name,
  value,
  options,
  allLabel,
}: {
  name: string;
  value: string;
  options: { value: string; label: string }[];
  allLabel: string;
}) {
  const params = useSearchParams();
  const router = useRouter();
  const pathname = usePathname();

  return (
    <select
      aria-label={allLabel}
      data-qa-sales-filter={name}
      value={value}
      onChange={(event) => {
        const next = new URLSearchParams(params.toString());
        if (event.target.value === "") next.delete(name);
        else next.set(name, event.target.value);
        const text = next.toString();
        router.replace(text ? `${pathname}?${text}` : pathname, { scroll: false });
      }}
      className="rounded-md border border-line bg-panel px-2 py-1.5 text-[12.5px] outline-none"
    >
      <option value="">{allLabel}</option>
      {options.map((option) => (
        <option key={option.value} value={option.value}>
          {option.label}
        </option>
      ))}
    </select>
  );
}
