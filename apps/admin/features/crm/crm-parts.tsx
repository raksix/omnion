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

/**
 * The props that draw the keyboard cursor on a row.
 *
 * `j` and `k` move a cursor that only existed as a number in the frame's state: no screen read
 * `selectedIndex`, so pressing `j` changed which row `Enter` would open while nothing on the page
 * said so. A shortcut you cannot see is indistinguishable from a broken one, and the person
 * pressing it is guessing. Every list row therefore draws the cursor, which is also what lets the
 * QA pass find the cursor and press `Enter` against a row it can name.
 */
function crmRowCursor(selected: boolean): {
  "data-qa-crm-cursor": string;
  "aria-selected"?: true;
  className: string;
} {
  return selected
    ? {
        "data-qa-crm-cursor": "true",
        "aria-selected": true,
        className: "bg-accent-soft/50 ring-1 ring-inset ring-accent/30",
      }
    : { "data-qa-crm-cursor": "false", className: "" };
}

/**
 * One row of a CRM list, with the keyboard cursor on it.
 *
 * This is a component rather than a helper function on purpose. A list's rows are the `children`
 * its screen passes to the frame, and `children` is built *outside* the provider — so a screen
 * cannot read `selectedIndex` where it writes the row, however it wants the markup to look. The
 * row reads the context itself, which is the only place the cursor is actually available, and it
 * also moves the cursor on click: pressing `j` and clicking are the same act, and a click that
 * leaves the cursor somewhere else makes the two disagree.
 */
export function CrmRow({
  index,
  className,
  onClick,
  children,
}: {
  /** The row's position in the list, which is what `j`/`k` count in. */
  index: number;
  /** The screen's own classes, merged after the cursor's. */
  className?: string;
  /**
   * What a click on the row does.
   *
   * A row component that hard-codes its own click would have to know what a row *is*, which is
   * the one thing the frame does not know — so the screen says, and the row still owns the cursor.
   * Clicking and pressing `j` are the same act, so both end in `setSelectedIndex`; without this
   * the click moved the cursor and the screen's own handler did something else entirely, which is
   * how a list ends up selecting one row and opening another.
   */
  onClick?: () => void;
  /** The cells. */
  children: ReactNode;
}) {
  const list = useCrmList();
  const cursor = crmRowCursor(index === list.selectedIndex);
  return (
    <tr
      {...cursor}
      onClick={() => {
        list.setSelectedIndex(index);
        onClick?.();
      }}
      className={[cursor.className, className].filter(Boolean).join(" ")}
    >
      {children}
    </tr>
  );
}

/**
 * Where the `g` prefix lands, and nothing else.
 *
 * Built from the tab bar above rather than typed out a second time, so a section the nav has and
 * the keyboard cannot reach — or the reverse — is a difference in one place instead of two lists
 * that drift. `c`/`o` are answered by the same prefix as every other destination: Contacts and
 * Companies are where a person already is on those two screens, so a "go there" row there would
 * be a shortcut that goes nowhere.
 */
const GO_DESTINATIONS: Record<string, string> = Object.fromEntries(
  CRM_NAV.filter((item) => item.shortcut !== "c" && item.shortcut !== "o").map((item) => [
    item.shortcut,
    item.href,
  ]),
);

/**
 * The keyboard contract of a list screen.
 *
 * Every row here is a binding some screen actually implements, which is the rule this list is
 * written against: the sheet is opened *by* a key, so a row that promises a binding nobody
 * listens for is a control the screen offers and cannot deliver — a dead list inside the screen
 * whose whole job is to report the screen truthfully. (`c` and `o` were advertised here for
 * several ticks while no listener existed anywhere in the module; they are now real navigations
 * and are exercised by the keyboard pass.)
 */
export const LIST_SHORTCUTS: Shortcut[] = [
  { keys: "/", what: "Focus the search" },
  { keys: "j / k", what: "Move to the next / previous row" },
  { keys: "Enter", what: "Open the selected row" },
  { keys: "e", what: "Edit the selected row" },
  { keys: "n", what: "Create a record" },
  { keys: "g then d", what: "Go to Deals" },
  { keys: "g then a", what: "Go to Activities" },
  { keys: "g then l", what: "Go to Leads" },
  { keys: "g then s", what: "Go to Settings" },
  { keys: "?", what: "Show or hide this sheet" },
];

/**
 * The keyboard contract, as a hook rather than a body inside the frame.
 *
 * It used to be written out inside `CrmShell`, which is why `/crm/activities` and `/crm/leads` have
 * none of it: neither renders the shell (the activity feed is a form with a list beneath it, the
 * lead inbox carries its own settings), so the contract was reachable only by the screens that
 * happened to sit inside the one component that inlined it. The shortcut sheet is the **module's**
 * claim about what a CRM screen listens for, and it was untrue of half the module's screens.
 *
 * So the bindings live here once, and a screen that draws its own rows calls this hook. What stays
 * with the screen is what a row *means*: `onOpen` and `onEdit` are its own functions, because this
 * hook cannot know what a lead's id is. `Enter` and `e` are both required and are deliberately
 * allowed to be the same call — a deal on a board has no separate view route, so on every CRM
 * screen opening a row and editing it are one action, and the sheet promises two keys for it.
 *
 * `g` is the "go" prefix: the first key arms it and the second chooses the destination. It is
 * deliberately **not** a bare letter, because a bare `c`/`o`/`d` has to type into a search box and
 * edit a row, and a sheet that claims a bare letter nobody listens for is the dead list this file
 * exists to prevent.
 */
export function useCrmKeyboard(props: {
  /** The rows the cursor may land on, in display order — `j`/`k` count exactly these. */
  rowIds: string[];
  /** A ref for the search field, so `/` and the palette can both focus it. */
  searchRef: React.RefObject<HTMLInputElement | null>;
  /** Open the row the cursor is on. */
  onOpen: (id: string) => void;
  /** Edit the row the cursor is on. */
  onEdit: (id: string) => void;
  /** Open the create form. */
  onCreate: () => void;
}): {
  /** The row the cursor is on. */
  selectedIndex: number;
  /** Move the cursor to an absolute position. */
  setSelectedIndex: (value: number) => void;
  /** Move the cursor by a delta, wrapping at both ends. */
  select: (delta: number) => void;
  /** `true` when the sheet is showing. */
  showShortcuts: boolean;
  /** Show or hide the sheet — the toolbar's keyboard button calls this. */
  toggleShortcuts: () => void;
} {
  const { rowIds, searchRef, onOpen, onEdit, onCreate } = props;
  const router = useRouter();
  const [showShortcuts, setShowShortcuts] = useState(false);
  const [selectedIndex, setSelectedIndex] = useState(0);
  const [pendingG, setPendingG] = useState(false);

  // `j`/`k` wrap at the ends, because a list that stops at the last row leaves a person unsure
  // whether the list ended or the shortcut did. The toolbar's "move by N" uses this too, so the
  // wrap rule lives in one place instead of being re-derived by a second caller.
  const select = useCallback(
    (delta: number) =>
      setSelectedIndex((index) =>
        rowIds.length === 0 ? 0 : (index + delta + rowIds.length) % rowIds.length,
      ),
    [rowIds.length],
  );

  useEffect(() => {
    if (!pendingG) return;
    const timer = window.setTimeout(() => setPendingG(false), 1200);
    return () => window.clearTimeout(timer);
  }, [pendingG]);

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
      // The armed `g` is answered before anything else, so a pending prefix cannot leave `d`
      // looking like an unhandled key.
      if (pendingG) {
        const destination = GO_DESTINATIONS[event.key.toLowerCase()];
        setPendingG(false);
        if (destination) {
          event.preventDefault();
          router.push(destination);
        }
        return;
      }
      if (event.key === "g") {
        event.preventDefault();
        setPendingG(true);
        return;
      }
      if (event.key === "?") {
        event.preventDefault();
        setShowShortcuts((open) => !open);
        return;
      }
      if (event.key === "j" || event.key === "ArrowDown") {
        event.preventDefault();
        select(1);
        return;
      }
      if (event.key === "k" || event.key === "ArrowUp") {
        event.preventDefault();
        select(-1);
        return;
      }
      if (event.key === "n") {
        event.preventDefault();
        onCreate();
        return;
      }
      if (event.key === "e") {
        const id = rowIds[selectedIndex];
        if (id) {
          event.preventDefault();
          onEdit(id);
        }
        return;
      }
      if (event.key === "Enter") {
        const id = rowIds[selectedIndex];
        if (id) {
          event.preventDefault();
          onOpen(id);
        }
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onCreate, onEdit, onOpen, pendingG, router, rowIds, searchRef, select, selectedIndex]);
  // A filter that leaves no row must not leave the keyboard pointing at a row that is gone.
  useEffect(() => {
    if (rowIds.length === 0) {
      setSelectedIndex(0);
    } else if (selectedIndex >= rowIds.length) {
      setSelectedIndex(rowIds.length - 1);
    }
  }, [rowIds, selectedIndex]);

  const toggleShortcuts = useCallback(() => setShowShortcuts((open) => !open), []);

  return { selectedIndex, setSelectedIndex, select, showShortcuts, toggleShortcuts };
}

/** The sheet the `?` key opens, drawn identically by every frame in the module. */
export function CrmShortcutSheet() {
  return (
    <div data-qa="crm-shortcut-sheet" className="border-b border-line bg-canvas/60 px-4 py-3">
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
  );
}

/** The cursor props for a frame that renders `<li>` rows rather than table rows. */
export function crmListItemCursor(selected: boolean): {
  "data-qa-crm-cursor": "true" | "false";
  "aria-selected": true | undefined;
  className: string;
} {
  return selected
    ? {
        "data-qa-crm-cursor": "true",
        "aria-selected": true,
        className: "bg-accent-soft/50 ring-1 ring-inset ring-accent/30",
      }
    : { "data-qa-crm-cursor": "false", "aria-selected": undefined, className: "" };
}

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
  /** A ref for the search field, so `/` and the palette can both focus it. */
  searchRef: React.RefObject<HTMLInputElement | null>;
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
  /** What the keyboard actions do — supplied by the screen, which owns the rows.
   *
   *  `onOpen` and `onEdit` are both required and are answered by the same function on every screen
   *  that has rows: a deal on the board has no separate view route, so opening a row and editing it
   *  are one action. They are kept as two names because the *sheet* promises two keys, and a key
   *  that has no argument to call is a binding nobody listens for. `onCreate` is here rather than
   *  being the frame's own `onCreate` prop so that `n` and the "New" button take one path. */
  keyboard: {
    onEdit: (id: string) => void;
    onOpen: (id: string) => void;
    onCreate: () => void;
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
  // The bindings themselves live in `useCrmKeyboard`, because they are the module's claim rather
  // than this frame's: `/crm/activities` and `/crm/leads` draw their own rows and answer to the
  // same sheet, and an earlier version inlined the whole contract here, which left those two
  // screens silently un-keyboardable while the sheet still promised them keys.
  const requestCreate = useCallback(() => {
    keyboard.onCreate();
  }, [keyboard]);

  const { selectedIndex, setSelectedIndex, select, showShortcuts, toggleShortcuts } =
    useCrmKeyboard({
      rowIds,
      searchRef,
      onOpen: keyboard.onOpen,
      onEdit: keyboard.onEdit,
      onCreate: requestCreate,
    });
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

  // The keyboard contract is answered by the **screen**, through the `keyboard` prop it already
  // passes: a contact list and a deals board open their editors in different ways, and the frame
  // is the only place that does not know what a row's id *is* on either of them.
  //
  // An earlier version published a second channel — `formRequest` in this context, read by nobody
  // — and had to answer a type error with `as never` when it wanted an `"open"` kind that the type
  // did not carry. That is the shape of a dead control: it is declared, it is typed, it is put on
  // the context where a reader would look for it, and nothing ever reads it. `n` used to set it
  // *and* call `onCreate`, so the create form opened twice over on any screen that also read it.

  // `Enter` and `e` are the same call: a deal on a board has no separate view route, so opening a
  // row and editing it are one action, and a contacts list routes both to its own editor.
  const requestEdit = useCallback((id: string) => keyboard.onEdit(id), [keyboard]);

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

  // Move the cursor into view, so `j` past the bottom of the screen follows the eye. `nearest`
  // scrolls only when the row is actually out of view, which is what keeps a person reading the
  // list from having the page jump under them on every key press.
  //
  // The row is found by the attribute `CrmRow` writes rather than by an id this file guesses at:
  // each screen names its rows its own way (`crm-contact-…`, `crm-company-…`), and a lookup that
  // assumed one of them would scroll nothing at all and look like it had worked.
  useEffect(() => {
    if (rowIds.length === 0) return;
    const cursor = document.querySelector('[data-qa-crm-cursor="true"]');
    cursor?.scrollIntoView({ block: "nearest", inline: "nearest" });
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
    select,
    selectedIndex,
    setSelectedIndex,
    requestCreate,
    requestEdit,
    searchRef,
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
              onClick={toggleShortcuts}
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

        {showShortcuts ? <CrmShortcutSheet /> : null}

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
