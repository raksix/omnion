"use client";

/**
 * The project switcher (REQ-133, acceptance 3) — the automation header's "which project" control.
 *
 * Acceptance 3 asks for four things and the four of them are four different mechanisms, which is
 * why this is one component and not a `<select>`:
 *
 * 1. **Recents first, and the order is the server's.** `recent_rank` arrives ranked and the panel
 *    renders it in the order it was given. A client-side "sort by last opened" would need the
 *    recents, and a browser that knows only its own `localStorage` history is exactly the
 *    two-lists-drift the API comment warns about.
 * 2. **Typing filters.** A nine-project installation is a `<select>`; a nine-hundred-project one is
 *    a search box, and the switcher is the only place the reader can reach a project at all — a
 *    control that cannot be searched is a control that has become a list.
 * 3. **`p` opens it, and Escape closes it.** A keyboard shortcut that is documented and not
 *    implemented is worse than none; `p` is chosen because it is unbound in the panel and because
 *    "project" and "p" agree in every language the panel is read in.
 * 4. **The selection is durable, and the URL is the view.** Two separate facts, and this component
 *    is where people conflate them: a *shared link* carries `?project=<id>` so a colleague opens
 *    what you were looking at, while the *stored selection* is yours and survives a reload. The
 *    switcher marks the URL's project when the URL names one, and writes the stored selection when
 *    you actually choose — so following a colleague's link does not overwrite your own default.
 *
 * **"All projects" is an entry, not the absence of one.** A caller who has never chosen is in the
 * same *state* as a caller who chose "everything", and the API answers both with `selected: null`.
 * The panel says which: an unchosen reader is offered the entry, a reader who chose it is marked.
 * Those are different messages and the difference is worth a line of UI, because the first means
 * "your choice is not saved anywhere" and the second means "you are not looking at one project".
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { usePathname, useRouter } from "next/navigation";
import { Check, FolderKanban, Loader2, Search, TriangleAlert, X } from "lucide-react";

import { ApiError, fetchProjectSwitcher, selectProject } from "@/lib/api";
import type { ProjectSwitcher, ProjectSwitcherEntry } from "@/lib/types";

const ROLE_LABEL: Record<string, string> = {
  owner: "Owner",
  editor: "Editor",
  operator: "Operator",
  viewer: "Viewer",
};

/** The URL parameter a shared link carries. The same name the REQ's API table uses. */
const PROJECT_PARAM = "project";

/**
 * The automation routes the switcher filters.
 *
 * **The switcher only appears where a project actually scopes the screen.** On `/pages` a project
 * has no meaning — offering a control there would make the reader believe their pages are in a
 * bucket, which is the belief this REQ is built to remove. So the check is a path prefix, and it
 * is a *list* rather than a set of project screens because the automation surface is growing:
 * `/automation/…` plus the screens whose contents are project-scoped.
 */
const SCOPED_PREFIXES = ["/automation", "/workflows", "/events", "/webhooks", "/ai"];

function isProjectScoped(pathname: string | null): boolean {
  if (!pathname) return false;
  return SCOPED_PREFIXES.some((prefix) => pathname === prefix || pathname.startsWith(`${prefix}/`));
}

/** Read the project a shared link names, and whether it is a real id rather than junk. */
function projectFromUrl(params: URLSearchParams): string | null {
  const value = params.get(PROJECT_PARAM);
  if (!value) return null;
  // A uuid, and nothing else. Rendering a shared link's `?project=<script>` as a selection would
  // put the string in the DOM, and the panel's own ids are uuids, so anything else is a broken
  // link rather than a project.
  return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(value)
    ? value
    : null;
}

export function ProjectSwitcher() {
  const router = useRouter();
  const pathname = usePathname();
  const scoped = isProjectScoped(pathname);

  const [data, setData] = useState<ProjectSwitcher | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [busy, setBusy] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  // The URL is read through `window.location` rather than `useSearchParams` on purpose: a shared
  // link's `?project=` must be readable on a screen whose content is *not* project-scoped too, and
  // `useSearchParams` in a client component opts the whole subtree into a Suspense boundary that
  // the automation screens do not currently have. The value is read on open rather than on every
  // render, because the switcher reads it exactly once per open.
  const [urlProject, setUrlProject] = useState<string | null>(null);

  const load = useCallback(async () => {
    if (!scoped) return;
    setError(null);
    try {
      setData(await fetchProjectSwitcher());
    } catch (cause) {
      setData(null);
      setError(cause instanceof ApiError ? cause.message : "your projects could not be read");
    }
  }, [scoped]);

  useEffect(() => {
    void load();
  }, [load]);

  // Clicking away closes the sheet. A control that stays open behind a menu you opened elsewhere
  // reads as a stuck overlay, and Escape is the keyboard equivalent of the same action.
  useEffect(() => {
    if (!open) return;
    const onPointerDown = (event: MouseEvent) => {
      if (rootRef.current && !rootRef.current.contains(event.target as Node)) setOpen(false);
    };
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") setOpen(false);
    };
    document.addEventListener("mousedown", onPointerDown);
    document.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("mousedown", onPointerDown);
      document.removeEventListener("keydown", onKeyDown);
    };
  }, [open]);

  // `p` opens it, from anywhere on an automation screen, and never while the reader is typing
  // into something else: a bare `p` listener turns the letter p in a search box into a navigation.
  useEffect(() => {
    if (!scoped) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key.toLowerCase() !== "p") return;
      const target = event.target as HTMLElement | null;
      if (target && ["INPUT", "TEXTAREA", "SELECT"].includes(target.tagName)) return;
      if (target?.isContentEditable) return;
      if (event.metaKey || event.ctrlKey || event.altKey) return;
      event.preventDefault();
      setOpen((value) => !value);
    };
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [scoped]);

  useEffect(() => {
    if (open) inputRef.current?.focus();
  }, [open]);

  const entries = useMemo(() => data?.projects ?? [], [data]);

  // Recents first, then the rest — and the *server* order is kept within each group. Sorting the
  // rest by name here would be a second opinion about an ordering the API already decided, and the
  // two would disagree the first time a project was renamed.
  const { recents, others } = useMemo(() => {
    const needle = query.trim().toLowerCase();
    const matches = entries.filter(
      (entry) =>
        needle === "" ||
        entry.name.toLowerCase().includes(needle) ||
        entry.key.toLowerCase().includes(needle) ||
        entry.description.toLowerCase().includes(needle),
    );
    return {
      recents: matches.filter((entry) => entry.recent_rank !== null),
      others: matches.filter((entry) => entry.recent_rank === null),
    };
  }, [entries, query]);

  if (!scoped) return null;

  // What the button names. The URL wins over the stored selection, because a link is a request to
  // look at something — but the stored one is the fallback, so a plain navigation lands you where
  // you last were.
  const activeId = urlProject ?? data?.selected ?? null;
  const active = entries.find((entry) => entry.id === activeId) ?? null;

  const choose = useCallback(
    async (projectId: string | null) => {
      setBusy(true);
      setError(null);
      try {
        // The server's answer replaces the local one rather than being merged into it: the recents
        // it ranks are the fact, and a client that patched its own list would have to reproduce
        // the demotion the store just did.
        setData(await selectProject(projectId));
        setOpen(false);
        setQuery("");

        // The URL is the *view*, so the switch writes it too — a shared link reproduces what the
        // sender was looking at. The stored selection is what a plain navigation restores.
        const params = new URLSearchParams(window.location.search);
        if (projectId) params.set(PROJECT_PARAM, projectId);
        else params.delete(PROJECT_PARAM);
        const suffix = params.toString() ? `?${params.toString()}` : "";
        router.replace(`${pathname ?? "/"}${suffix}`);
      } catch (cause) {
        setError(cause instanceof ApiError ? cause.message : "the switch could not be saved");
      } finally {
        setBusy(false);
      }
    },
    [pathname, router],
  );

  // Skeleton: the control's shape is a pill with a name in it, and a control that is absent while
  // it loads makes the header jump. The same shape, empty, for the same reason `SiteSwitcher` does
  // it.
  if (error && !data) {
    return (
      <div ref={rootRef} className="relative">
        <button
          type="button"
          data-testid="project-switcher-error"
          onClick={() => void load()}
          className="flex items-center gap-2 rounded-lg border border-dashed border-line px-2.5 py-2 text-[12px] text-muted"
        >
          <TriangleAlert className="size-3.5" aria-hidden />
          Projects unavailable — retry
        </button>
      </div>
    );
  }

  if (!data) {
    return <span aria-hidden className="h-9 w-44 animate-pulse rounded-lg bg-quiet-soft" />;
  }

  return (
    <div ref={rootRef} className="relative">
      <button
        type="button"
        data-testid="project-switcher"
        aria-haspopup="listbox"
        aria-expanded={open}
        onClick={() => {
          setUrlProject(projectFromUrl(new URLSearchParams(window.location.search)));
          setOpen((value) => !value);
        }}
        className="flex items-center gap-2 rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px] transition hover:bg-quiet-soft"
      >
        <FolderKanban className="size-3.5 text-muted" aria-hidden />
        {active ? (
          <span className="max-w-32 truncate font-medium" data-testid="project-switcher-current">
            {active.name}
          </span>
        ) : (
          <span className="text-muted" data-testid="project-switcher-current">
            All projects
          </span>
        )}
        <span className="text-[11px] text-muted" aria-hidden>
          p
        </span>
      </button>

      {error ? (
        <p
          role="alert"
          data-testid="project-switcher-strip"
          className="absolute right-0 top-full z-40 mt-1.5 w-72 rounded-lg border border-line bg-surface p-2.5 text-[12px] text-muted"
        >
          {error}
        </p>
      ) : null}

      {open ? (
        <div
          role="listbox"
          aria-label="Switch project"
          data-testid="project-switcher-panel"
          className="absolute right-0 top-full z-40 mt-1.5 w-80 overflow-hidden rounded-xl border border-line bg-surface shadow-lg"
        >
          <div className="flex items-center gap-2 border-b border-line px-3 py-2">
            <Search className="size-3.5 shrink-0 text-muted" aria-hidden />
            <input
              ref={inputRef}
              data-testid="project-switcher-search"
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              placeholder="Filter projects…"
              aria-label="Filter projects"
              className="w-full bg-transparent text-[12.5px] outline-none placeholder:text-muted"
            />
            {query ? (
              <button
                type="button"
                onClick={() => setQuery("")}
                aria-label="Clear the filter"
                className="text-muted hover:text-ink"
              >
                <X className="size-3.5" aria-hidden />
              </button>
            ) : null}
          </div>

          <div className="max-h-80 overflow-y-auto py-1">
            {/* "All projects" is offered while it is not what you are looking at, and marked while
                it is. It is never hidden: a control that can switch you *out* of a project and gives
                no way back is a one-way door. */}
            <SwitcherRow
              label="All projects"
              hint="Every project you can see"
              selected={activeId === null}
              disabled={busy}
              onSelect={() => void choose(null)}
              testid="project-switcher-all"
            />

            {recents.length > 0 ? <SectionLabel label="Recent" /> : null}
            {recents.map((entry) => (
              <SwitcherRow
                key={entry.id}
                entry={entry}
                selected={entry.id === activeId}
                disabled={busy}
                onSelect={() => void choose(entry.id)}
              />
            ))}

            {others.length > 0 ? <SectionLabel label="All your projects" /> : null}
            {others.map((entry) => (
              <SwitcherRow
                key={entry.id}
                entry={entry}
                selected={entry.id === activeId}
                disabled={busy}
                onSelect={() => void choose(entry.id)}
              />
            ))}

            {recents.length === 0 && others.length === 0 ? (
              <p className="px-3 py-6 text-center text-[12px] text-muted" data-testid="project-switcher-empty">
                {query.trim() === ""
                  ? "You are not a member of any project yet"
                  : `Nothing matches “${query.trim()}”`}
              </p>
            ) : null}
          </div>

          {busy ? (
            <p className="flex items-center gap-2 border-t border-line px-3 py-2 text-[11.5px] text-muted">
              <Loader2 className="size-3.5 animate-spin" aria-hidden />
              Saving your switch…
            </p>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}

function SectionLabel({ label }: { label: string }) {
  return (
    <p className="px-3 pb-1 pt-2.5 text-[10.5px] font-semibold uppercase tracking-wide text-muted">
      {label}
    </p>
  );
}

function SwitcherRow({
  entry,
  label,
  hint,
  selected,
  disabled,
  onSelect,
  testid,
}: {
  entry?: ProjectSwitcherEntry;
  /** For the synthetic "All projects" row, which is not a project. */
  label?: string;
  hint?: string;
  selected: boolean;
  disabled: boolean;
  onSelect: () => void;
  testid?: string;
}) {
  const name = entry ? entry.name : (label ?? "");
  const meta = entry
    ? [
        entry.key,
        entry.caller_role ? ROLE_LABEL[entry.caller_role] : entry.can_manage ? "Administrator" : null,
        entry.is_default ? "Default" : null,
        entry.status === "archived" ? "Archived" : null,
      ]
        .filter(Boolean)
        .join(" · ")
    : (hint ?? "");

  return (
    <button
      type="button"
      role="option"
      aria-selected={selected}
      data-testid={testid ?? `project-switcher-row-${entry?.key ?? ""}`}
      disabled={disabled}
      onClick={onSelect}
      className="flex w-full items-center gap-2.5 px-3 py-2 text-left transition hover:bg-quiet-soft disabled:opacity-60"
    >
      {entry?.color ? (
        <span
          aria-hidden
          className="size-2.5 shrink-0 rounded-full"
          style={{ backgroundColor: entry.color }}
        />
      ) : (
        <span aria-hidden className="size-2.5 shrink-0 rounded-full bg-quiet-soft" />
      )}
      <span className="flex min-w-0 flex-1 flex-col leading-tight">
        <span className="truncate text-[12.5px] font-medium">{name}</span>
        {meta ? <span className="truncate text-[11px] text-muted">{meta}</span> : null}
      </span>
      {selected ? <Check className="size-3.5 shrink-0 text-accent" aria-hidden /> : null}
    </button>
  );
}
