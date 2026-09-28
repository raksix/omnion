"use client";

/**
 * The frame the `/crm` screens share (REQ-051, slice 2).
 *
 * One second-level tab bar under the app shell, one toolbar for the filters every list has, and
 * the keyboard contract the spec asks for: `/` reaches the search, `j`/`k` move the selected row,
 * `e` edits it, `Enter` opens it, `n` starts a new record and `?` shows the shortcut sheet. The
 * shortcuts are ignored while a field has focus — a form is not a keyboard surface for the list
 * behind it.
 *
 * The list's state lives in the **URL**, not in a component: a filtered list is a link, so the
 * back button, a bookmark and a shared URL all land on the same rows. That is also what makes a
 * saved view work — a view is the same fields, and applying it rewrites the URL.
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

/** The screens of the CRM section. `Overview` and the rest arrive with the later slices. */
export const CRM_NAV = [
  { href: "/crm/contacts", label: "Contacts", shortcut: "c" },
  { href: "/crm/companies", label: "Companies", shortcut: "o" },
  { href: "/crm/deals", label: "Deals", shortcut: "d" },
  { href: "/crm/activities", label: "Activities", shortcut: "a" },
  { href: "/crm/leads", label: "Leads", shortcut: "l" },
  { href: "/crm/settings/pipelines", label: "Settings", shortcut: "s" },
] as const;

/** One shortcut, as the sheet prints it. */
type Shortcut = { keys: string; what: string };

/** The keyboard contract of a list screen. */
const LIST_SHORTCUTS: Shortcut[] = [
  { keys: "/", what: "Focus the search" },
  { keys: "j / k", what: "Move to the next / previous row" },
  { keys: "Enter", what: "Open the selected row" },
  { keys: "e", what: "Edit the selected row" },
  { keys: "n", what: "Create a record" },
  { keys: "c", what: "Go to Contacts" },
  { keys: "o", what: "Go to Companies" },
  { keys: "?", what: "Show or hide this sheet" },
];

/** What the toolbar and the list need from the frame. */
export type CrmListState = {
  /** The current search term. */
  search: string;
  /** Set the search term — the URL is rewritten, the list refetches. */
  setSearch: (value: string) => void;
  /** The current owner filter (`me`, `unassigned` or an account id). */
  owner: string;
  setOwner: (value: string) => void;
  /** The current status filter. */
  status: string;
  setStatus: (value: string) => void;
  /** The current tag filter. */
  tag: string;
  setTag: (value: string) => void;
  /** Whether the archived rows are shown. */
  includeArchived: boolean;
  setIncludeArchived: (value: boolean) => void;
  /** The sort key and direction. */
  sort: string;
  direction: "asc" | "desc";
  setSort: (key: string) => void;
  /** The columns the list shows, in order. */
  columns: string[];
  setColumns: (value: string[]) => void;
  /** The status values the filter may hold. */
  statuses: string[];
  /** Move the row selection. */
  select: (delta: number) => void;
  /** The row the keyboard has on. */
  selectedIndex: number;
  setSelectedIndex: (value: number) => void;
  /** Open the create form. */
  requestCreate: () => void;
  /** Open the edit form for a row. */
  requestEdit: (id: string) => void;
  /** Open a row. */
  requestOpen: (id: string) => void;
  /** A ref for the search field, so `/` and the palette can both focus it. */
  searchRef: React.RefObject<HTMLInputElement | null>;
  /** The create/edit request the screen above is carrying out. */
  formRequest: { kind: "create" | "edit"; id: string | null } | null;
  /** Answer a form request from the screen. */
  clearFormRequest: () => void;
  /** The row count the API reported, and the cursor of the next page. */
  total: number;
  nextCursor: string | null;
  /** Append the next page (the list pages with a cursor, not an offset). */
  loadMore: () => void;
  /** `true` while the next page is on its way. */
  loadingMore: boolean;
  /** The columns a list of this entity may show. */
  availableColumns: string[];
};

const CrmListContext = createContext<CrmListState | null>(null);

/** The toolbar's state from inside a list. */
export function useCrmList(): CrmListState {
  const value = useContext(CrmListContext);
  if (!value) {
    throw new Error("useCrmList must be used inside <CrmShell>");
  }
  return value;
}

// ---------------------------------------------------------------------------------------------
// The frame
// ---------------------------------------------------------------------------------------------

type CrmShellProps = {
  /** What the screen is. */
  title: string;
  /** One line about what the screen is for. */
  description: string;
  /**
   * Which list's state the toolbar holds.
   *
   * A closed set, and it is what decides which filters the toolbar draws: a deal has a stage, a
   * probability and a close date, and none of the contact-only filters (`tag`, `archived`, the
   * lifecycle `status`) mean anything for it. Rendering them greyed out would be a control that
   * looks real and does nothing.
   */
  entity: "contacts" | "companies" | "deals";
  /** The columns the entity may show, read from the API. */
  availableColumns: string[];
  /** The lifecycle statuses the entity has, read from the API. */
  statuses: string[];
  /** How many rows the list has, for the toolbar's own line. */
  total: number;
  /** The cursor of the next page, when there is one. */
  nextCursor: string | null;
  /** `true` while the next page is being fetched. */
  loadingMore: boolean;
  /** Append the next page. */
  loadMore: () => void;
  /** Open the create form. */
  onCreate: () => void;
  /** The screen's own body: the form, the table, the empty state. */
  children: ReactNode;
  /** What the toolbar offers beyond the shared controls. */
  toolbarExtra?: ReactNode;
  /** What the keyboard actions do — supplied by the screen, which owns the rows. */
  keyboard: {
    onEdit: (id: string) => void;
    onOpen: (id: string) => void;
  };
  /** The rows the keyboard may land on. */
  rowIds: string[];
};

/** The second-level nav and the shared toolbar every CRM list sits under. */
export function CrmShell(props: CrmShellProps) {
  const {
    title,
    description,
    entity,
    availableColumns,
    statuses,
    total,
    nextCursor,
    loadingMore,
    loadMore,
    onCreate,
    children,
    toolbarExtra,
    keyboard,
    rowIds,
  } = props;

  const router = useRouter();
  const pathname = usePathname();
  const searchParams = useSearchParams();
  const searchRef = useRef<HTMLInputElement | null>(null);
  const [showShortcuts, setShowShortcuts] = useState(false);
  const [selectedIndex, setSelectedIndex] = useState(0);
  const [formRequest, setFormRequest] = useState<CrmListState["formRequest"]>(null);
  const [columnMenuOpen, setColumnMenuOpen] = useState(false);

  const search = searchParams.get("search") ?? "";
  const owner = searchParams.get("owner") ?? "";
  const status = searchParams.get("status") ?? "";
  const tag = searchParams.get("tag") ?? "";
  const includeArchived = searchParams.get("include_archived") === "true";
  const sort = searchParams.get("sort") ?? "";
  const direction = (searchParams.get("direction") as "asc" | "desc") ?? "desc";
  // The column set travels in the URL too: a list with a column chooser is a link, and a shared
  // URL shows the columns the sender saw.
  const columns = useMemo(() => {
    const fromUrl = (searchParams.get("columns") ?? "")
      .split(",")
      .map((value) => value.trim())
      .filter((value) => value.length > 0 && availableColumns.includes(value));
    return fromUrl.length > 0 ? fromUrl : availableColumns;
    // `availableColumns` arrives once per screen; depending on the query is what refreshes it.
  }, [searchParams, availableColumns]);

  /** Rewrite one parameter of the list's URL, keeping the rest. */
  const setParam = useCallback(
    (key: string, value: string | null) => {
      const next = new URLSearchParams(searchParams.toString());
      if (value && value.length > 0) {
        next.set(key, value);
      } else {
        next.delete(key);
      }
      const query = next.toString();
      router.replace(query ? `${pathname}?${query}` : pathname, { scroll: false });
    },
    [pathname, router, searchParams],
  );

  const requestCreate = useCallback(() => {
    setFormRequest({ kind: "create", id: null });
    onCreate();
  }, [onCreate]);

  const requestEdit = useCallback((id: string) => setFormRequest({ kind: "edit", id }), []);
  const requestOpen = useCallback((id: string) => setFormRequest({ kind: "open", id } as never), []);

  const toggleColumn = useCallback(
    (column: string) => {
      const next = columns.includes(column)
        ? columns.filter((value) => value !== column)
        : [...columns, column];
      // A list with no columns would render a header row and nothing else, which reads as a
      // broken screen rather than a choice — so the last column cannot be removed.
      if (next.length === 0) {
        return;
      }
      setParam("columns", next.join(","));
    },
    [columns, setParam],
  );

  // The keyboard contract. `j`/`k` wrap at the ends, because a list that stops at the last row
  // leaves a person unsure whether the list ended or the shortcut did.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target &&
        (target.tagName === "INPUT" ||
          target.tagName === "TEXTAREA" ||
          target.tagName === "SELECT" ||
          target.isContentEditable);
      if (event.metaKey || event.ctrlKey || event.altKey) {
        return;
      }
      if (event.key === "/" && !typing) {
        event.preventDefault();
        searchRef.current?.focus();
        return;
      }
      if (typing) {
        return;
      }
      if (event.key === "?") {
        event.preventDefault();
        setShowShortcuts((open) => !open);
        return;
      }
      if (event.key === "j" || event.key === "ArrowDown") {
        event.preventDefault();
        setSelectedIndex((index) => (rowIds.length === 0 ? 0 : (index + 1) % rowIds.length));
        return;
      }
      if (event.key === "k" || event.key === "ArrowUp") {
        event.preventDefault();
        setSelectedIndex((index) =>
          rowIds.length === 0 ? 0 : (index - 1 + rowIds.length) % rowIds.length,
        );
        return;
      }
      if (event.key === "n") {
        event.preventDefault();
        requestCreate();
        return;
      }
      if (event.key === "e") {
        const id = rowIds[selectedIndex];
        if (id) {
          event.preventDefault();
          keyboard.onEdit(id);
        }
        return;
      }
      if (event.key === "Enter") {
        const id = rowIds[selectedIndex];
        if (id) {
          event.preventDefault();
          keyboard.onOpen(id);
        }
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [keyboard, requestCreate, rowIds, selectedIndex]);

  // A filter that leaves no row must not leave the keyboard pointing at a row that is gone.
  useEffect(() => {
    if (rowIds.length === 0) {
      setSelectedIndex(0);
    } else if (selectedIndex >= rowIds.length) {
      setSelectedIndex(rowIds.length - 1);
    }
  }, [rowIds, selectedIndex]);

  const value: CrmListState = {
    search,
    setSearch: (next) => setParam("search", next),
    owner,
    setOwner: (next) => setParam("owner", next),
    status,
    setStatus: (next) => setParam("status", next),
    tag,
    setTag: (next) => setParam("tag", next),
    includeArchived,
    setIncludeArchived: (next) => setParam("include_archived", next ? "true" : null),
    sort,
    direction,
    setSort: (key) => {
      if (key === sort) {
        setParam("direction", direction === "asc" ? "desc" : "asc");
        return;
      }
      const next = new URLSearchParams(searchParams.toString());
      next.set("sort", key);
      next.set("direction", "desc");
      const query = next.toString();
      router.replace(`${pathname}?${query}`, { scroll: false });
    },
    columns,
    setColumns: (next) => setParam("columns", next.join(",")),
    statuses,
    select: (delta) =>
      setSelectedIndex((index) =>
        rowIds.length === 0 ? 0 : (index + delta + rowIds.length) % rowIds.length,
      ),
    selectedIndex,
    setSelectedIndex,
    requestCreate,
    requestEdit,
    requestOpen,
    searchRef,
    formRequest,
    clearFormRequest: () => setFormRequest(null),
    total,
    nextCursor,
    loadMore,
    loadingMore,
    availableColumns,
  };

  return (
    <CrmListContext.Provider value={value}>
      <nav aria-label="CRM sections" className="flex flex-wrap items-center gap-1 border-b border-line pb-2">
        {CRM_NAV.map((item) => {
          const active = pathname === item.href || pathname.startsWith(`${item.href}/`);
          return (
            <Link
              key={item.href}
              href={item.href}
              aria-current={active ? "page" : undefined}
              className={`rounded-lg px-2.5 py-1.5 text-[12.5px] font-medium transition ${
                active
                  ? "bg-accent-soft text-accent-strong"
                  : "text-muted hover:bg-canvas hover:text-ink"
              }`}
            >
              {item.label}
            </Link>
          );
        })}
      </nav>

      <div className="mt-4 overflow-hidden rounded-xl border border-line bg-surface">
        <div className="flex flex-wrap items-end justify-between gap-3 border-b border-line px-4 py-3">
          <div className="flex flex-col gap-0.5">
            <h2 className="text-[13.5px] font-medium">{title}</h2>
            <span className="text-[12px] text-muted">{description}</span>
          </div>

          <div className="flex flex-wrap items-center gap-2">
            <label className="flex items-center gap-1.5">
              <span className="sr-only">Search {entity}</span>
              <input
                ref={searchRef}
                id={`crm-search-${entity}`}
                type="search"
                value={search}
                placeholder="Search…  ( / )"
                onChange={(event) => setParam("search", event.target.value || null)}
                className="w-44 rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
            </label>

            <label className="flex items-center gap-1.5">
              <span className="sr-only">Filter by owner</span>
              <select
                id={`crm-owner-${entity}`}
                value={owner}
                onChange={(event) => setParam("owner", event.target.value || null)}
                className="rounded-lg border border-line bg-surface px-2 py-1.5 text-[12px] outline-none focus:border-accent"
              >
                <option value="">Every owner</option>
                <option value="me">Owned by me</option>
                <option value="unassigned">Unassigned</option>
              </select>
            </label>

            {entity === "deals" ? null : (
            <label className="flex items-center gap-1.5">
              <span className="sr-only">Filter by status</span>
              <select
                id={`crm-status-${entity}`}
                value={status}
                onChange={(event) => setParam("status", event.target.value || null)}
                className="rounded-lg border border-line bg-surface px-2 py-1.5 text-[12px] outline-none focus:border-accent"
              >
                <option value="">Every status</option>
                {statuses.map((value_) => (
                  <option key={value_} value={value_}>
                    {value_.charAt(0).toUpperCase() + value_.slice(1)}
                  </option>
                ))}
              </select>
            </label>
            )}

            {entity === "deals" ? null : (
            <>
            <label className="flex items-center gap-1.5">
              <span className="sr-only">Filter by tag</span>
              <input
                id={`crm-tag-${entity}`}
                type="text"
                value={tag}
                placeholder="Tag"
                onChange={(event) => setParam("tag", event.target.value || null)}
                className="w-24 rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12px] outline-none transition focus:border-accent"
              />
            </label>

            <label className="flex items-center gap-1.5 text-[12px] text-muted">
              <input
                id={`crm-archived-${entity}`}
                type="checkbox"
                checked={includeArchived}
                onChange={(event) => setParam("include_archived", event.target.checked ? "true" : null)}
                className="size-3.5 rounded border-line"
              />
              Archived
            </label>
            </>
            )}

            <div className="relative">
              <button
                type="button"
                id={`crm-columns-${entity}`}
                aria-expanded={columnMenuOpen}
                onClick={() => setColumnMenuOpen((open) => !open)}
                className="rounded-lg border border-line px-2.5 py-1.5 text-[12px] transition hover:bg-canvas"
              >
                Columns
              </button>
              {columnMenuOpen ? (
                <div className="absolute right-0 z-20 mt-1 w-52 rounded-lg border border-line bg-surface p-2 shadow-lg">
                  <p className="px-1 pb-1.5 text-[11px] text-muted">
                    The chooser travels in the URL, so a shared link shows the same columns.
                  </p>
                  {availableColumns.map((column) => (
                    <label
                      key={column}
                      className="flex items-center gap-2 rounded px-1 py-1 text-[12px] hover:bg-canvas"
                    >
                      <input
                        type="checkbox"
                        checked={columns.includes(column)}
                        onChange={() => toggleColumn(column)}
                        className="size-3.5 rounded border-line"
                      />
                      {column.replace(/_/g, " ")}
                    </label>
                  ))}
                </div>
              ) : null}
            </div>

            <button
              type="button"
              onClick={() => setShowShortcuts((open) => !open)}
              aria-label="Keyboard shortcuts"
              className="rounded-lg border border-line p-2 text-muted transition hover:text-ink"
            >
              <Keyboard className="size-3.5" aria-hidden />
            </button>

            {toolbarExtra}

            <button
              type="button"
              data-qa-guard="crm-depth"
              onClick={requestCreate}
              className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
            >
              New {entity === "contacts" ? "contact" : entity === "companies" ? "company" : "deal"}
            </button>
          </div>
        </div>

        {showShortcuts ? (
          <div className="border-b border-line bg-canvas/60 px-4 py-3">
            <p className="pb-2 text-[11.5px] font-medium text-muted">Keyboard</p>
            <ul className="grid gap-1.5 sm:grid-cols-2">
              {LIST_SHORTCUTS.map((shortcut) => (
                <li key={shortcut.keys} className="flex items-center gap-2 text-[12px]">
                  <kbd className="rounded border border-line bg-surface px-1.5 py-0.5 font-mono text-[11px]">
                    {shortcut.keys}
                  </kbd>
                  <span className="text-muted">{shortcut.what}</span>
                </li>
              ))}
            </ul>
          </div>
        ) : null}

        {children}

        <div className="flex items-center justify-between border-t border-line px-4 py-2.5">
          <span className="text-[11.5px] text-muted">
            {total} {entity}
            {total === 1 || entity === "deals" ? "" : "s"} match
          </span>
          {nextCursor ? (
            <button
              type="button"
              onClick={loadMore}
              disabled={loadingMore}
              className="rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas disabled:opacity-50"
            >
              {loadingMore ? "Loading…" : "Load more"}
            </button>
          ) : null}
        </div>
      </div>
    </CrmListContext.Provider>
  );
}

// ---------------------------------------------------------------------------------------------
// The small pieces every CRM list draws
// ---------------------------------------------------------------------------------------------

/** An avatar circle with the record's initials. */
export function CrmAvatar({ initials, tone = "accent" }: { initials: string; tone?: "accent" | "quiet" }) {
  return (
    <span
      aria-hidden
      className={`flex size-7 shrink-0 items-center justify-center rounded-full text-[10.5px] font-semibold ${
        tone === "accent" ? "bg-accent-soft text-accent-strong" : "bg-quiet-soft text-muted"
      }`}
    >
      {initials || "?"}
    </span>
  );
}

/** One tag as a chip. */
export function CrmTag({ value }: { value: string }) {
  return (
    <span className="inline-flex items-center rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] text-muted">
      {value}
    </span>
  );
}

/** A lifecycle badge in the CRM's own vocabulary. */
const STATUS_TONES: Record<string, string> = {
  lead: "bg-caution-soft text-caution",
  customer: "bg-positive-soft text-positive",
  partner: "bg-accent-soft text-accent-strong",
  churned: "bg-quiet-soft text-muted",
};

/** The status pill of a contact or a company. */
export function CrmStatusBadge({ status }: { status: string }) {
  return (
    <span
      className={`inline-flex items-center rounded-full px-2 py-0.5 text-[11px] font-medium ${
        STATUS_TONES[status] ?? "bg-quiet-soft text-muted"
      }`}
    >
      {status.charAt(0).toUpperCase() + status.slice(1)}
    </span>
  );
}

/** A sortable column header: clicking it toggles the direction. */
export function CrmSortHeader({
  label,
  column,
  children,
}: {
  label: string;
  column: string;
  children: (state: { active: boolean; direction: "asc" | "desc" }) => ReactNode;
}) {
  const list = useCrmList();
  const active = list.sort === column;
  return (
    <th scope="col" className="px-3 py-2.5">
      <button
        type="button"
        onClick={() => list.setSort(column)}
        aria-sort={active ? (list.direction === "asc" ? "ascending" : "descending") : "none"}
        className={`inline-flex items-center gap-1 text-[11px] font-medium tracking-wide uppercase transition ${
          active ? "text-accent-strong" : "text-muted hover:text-ink"
        }`}
      >
        {children({ active, direction: list.direction })}
        <span aria-hidden className="text-[9px]">
          {active ? (list.direction === "asc" ? "▲" : "▼") : "↕"}
        </span>
        <span className="sr-only">{label}</span>
      </button>
    </th>
  );
}
