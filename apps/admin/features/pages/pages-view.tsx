"use client";

/**
 * Pages screen: the pages of the site the switcher is on, filterable by lifecycle state — and
 * the write side of the content model (docs/05-VERSIONING.md §4–§6): create a page, edit its
 * working draft, publish it. Editing appends a revision and publishing freezes it, so the list
 * always says which revision is live and which one is still waiting.
 */
import { useCallback, useEffect, useState } from "react";

import { Pencil, Plus, RefreshCw, Rocket } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { StatusBadge } from "@/components/status-badge";
import { ApiError, createPage, fetchPages, publishPage, updatePage } from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import { useSites } from "@/lib/sites";
import { pageTitle, type Page } from "@/lib/types";

const FILTERS = [
  { value: "", label: "All states" },
  { value: "draft", label: "Draft" },
  { value: "published", label: "Published" },
  { value: "archived", label: "Archived" },
] as const;

/** One row's second line: what is live and what is still waiting. */
function revisionNote(page: Page): string {
  const live = page.published ? `live v${page.published.revision_no}` : "not published";
  const pending = page.draft ? ` · draft v${page.draft.revision_no}` : "";
  return `${live}${pending}`;
}

/** The address a title suggests: `About Us!` → `about-us` (the shape the API accepts). */
function slugFor(title: string): string {
  return title
    .normalize("NFKD")
    .replace(/[\u0300-\u036f]/g, "")
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 96);
}

/** What the editor holds: the page it edits — `id: null` while creating one — and its content. */
type Editor = { id: string | null; title: string; slug: string; body: string };

/** The pages list with the create, edit and publish flow. */
export function PagesView() {
  const { selectedSite, status: sitesStatus } = useSites();
  const [filter, setFilter] = useState("");
  const [pages, setPages] = useState<Page[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [reloadToken, setReloadToken] = useState(0);

  // The editor — `null` while it is closed.
  const [editor, setEditor] = useState<Editor | null>(null);
  const [saving, setSaving] = useState(false);
  const [publishing, setPublishing] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const reload = useCallback(() => setReloadToken((token) => token + 1), []);

  useEffect(() => {
    if (!selectedSite) {
      setPages(null);
      setError(null);
      return;
    }

    let cancelled = false;
    setPages(null);
    setError(null);
    fetchPages(selectedSite.id, filter || undefined)
      .then((rows) => {
        if (!cancelled) {
          setPages(rows);
        }
      })
      .catch((cause: unknown) => {
        if (cancelled) {
          return;
        }
        setError(cause instanceof ApiError ? cause.message : "The pages could not be loaded.");
      });

    return () => {
      cancelled = true;
    };
  }, [selectedSite, filter, reloadToken]);

  const openCreate = () => {
    setActionError(null);
    setNotice(null);
    setEditor({ id: null, title: "", slug: "", body: "" });
  };

  const openEdit = (page: Page) => {
    setActionError(null);
    setNotice(null);
    setEditor({
      id: page.id,
      title: pageTitle(page),
      slug: page.slug,
      body: page.draft?.body ?? page.published?.body ?? "",
    });
  };

  /** Save the editor: a new page, or the next draft revision of the one being edited. */
  const save = async () => {
    if (!editor || saving || !selectedSite) {
      return;
    }
    const title = editor.title.trim();
    const slug = editor.slug.trim() || slugFor(title);
    if (!title) {
      setActionError("A title is required.");
      return;
    }
    if (!slug) {
      setActionError("A slug is required — letters, digits and dashes.");
      return;
    }

    setSaving(true);
    setActionError(null);
    setNotice(null);
    try {
      if (editor.id) {
        await updatePage(editor.id, { title, slug, body: editor.body });
        setNotice(`“${title}” saved as a new draft revision.`);
      } else {
        await createPage({ siteId: selectedSite.id, title, slug, body: editor.body });
        setNotice(`“${title}” created as a draft. Publish it to put it on the site.`);
      }
      setEditor(null);
      reload();
    } catch (cause: unknown) {
      setActionError(
        cause instanceof ApiError ? cause.message : "The page could not be saved.",
      );
    } finally {
      setSaving(false);
    }
  };

  /** Publish the working draft of one page. */
  const publish = async (page: Page) => {
    if (publishing) {
      return;
    }
    setPublishing(page.id);
    setActionError(null);
    setNotice(null);
    try {
      await publishPage(page.id);
      setNotice(`“${pageTitle(page)}” is live at /${page.slug}.`);
      reload();
    } catch (cause: unknown) {
      setActionError(
        cause instanceof ApiError ? cause.message : "The page could not be published.",
      );
    } finally {
      setPublishing(null);
    }
  };

  if (sitesStatus === "error") {
    return (
      <div className="rounded-xl border border-line bg-surface">
        <EmptyState
          title="The site list could not be loaded"
          hint="The pages screen follows the selected site, so it needs the sites first."
        />
      </div>
    );
  }

  if (sitesStatus === "ready" && !selectedSite) {
    return (
      <div className="rounded-xl border border-line bg-surface">
        <EmptyState
          title="No sites yet"
          hint="A site is where pages live. Create the first one through POST /api/v1/sites and it appears here."
        />
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-4">
      {actionError ? (
        <p
          role="alert"
          className="rounded-xl border border-caution/40 bg-caution-soft px-4 py-3 text-[12.5px]"
        >
          {actionError}
        </p>
      ) : null}
      {notice ? (
        <p className="rounded-xl border border-line bg-surface px-4 py-3 text-[12.5px] text-muted">
          {notice}
        </p>
      ) : null}

      {editor ? (
        <form
          className="overflow-hidden rounded-xl border border-line bg-surface"
          onSubmit={(event) => {
            event.preventDefault();
            void save();
          }}
        >
          <div className="flex items-baseline gap-2 border-b border-line px-4 py-3">
            <h2 className="text-[13.5px] font-medium">
              {editor.id ? "Edit page" : "New page"}
            </h2>
            <span className="text-[12px] text-muted">
              {editor.id
                ? "Saving appends a new draft revision; publishing freezes it."
                : "It starts as a draft — visitors see it once it is published."}
            </span>
          </div>
          <div className="flex flex-col gap-3 px-4 py-4">
            <div className="grid gap-3 sm:grid-cols-2">
              <label className="flex flex-col gap-1">
                <span className="text-[12px] font-medium">Title</span>
                <input
                  id="page-title"
                  name="title"
                  value={editor.title}
                  onChange={(event) =>
                    setEditor((current) =>
                      current ? { ...current, title: event.target.value } : current,
                    )
                  }
                  placeholder="e.g. About us"
                  required
                  className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
                />
              </label>
              <label className="flex flex-col gap-1">
                <span className="text-[12px] font-medium">Slug</span>
                <input
                  id="page-slug"
                  name="slug"
                  value={editor.slug}
                  onChange={(event) =>
                    setEditor((current) =>
                      current ? { ...current, slug: event.target.value } : current,
                    )
                  }
                  placeholder="about-us"
                  className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 font-mono text-[12px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
                />
                <span className="text-[11px] text-muted">
                  The address inside the site; derived from the title while it is empty.
                </span>
              </label>
            </div>
            <label className="flex flex-col gap-1">
              <span className="text-[12px] font-medium">Body</span>
              <textarea
                id="page-body"
                name="body"
                value={editor.body}
                onChange={(event) =>
                  setEditor((current) =>
                    current ? { ...current, body: event.target.value } : current,
                  )
                }
                rows={6}
                placeholder="Write the page. Plain text for now — the block editor arrives with a later phase."
                className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
            </label>
            <div className="flex flex-wrap items-center gap-2">
              <button
                type="submit"
                disabled={saving}
                className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-quiet-soft disabled:text-muted"
              >
                {editor.id ? "Save draft" : "Create page"}
              </button>
              <button
                type="button"
                onClick={() => setEditor(null)}
                className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
              >
                Cancel
              </button>
            </div>
          </div>
        </form>
      ) : null}

      <div className="overflow-hidden rounded-xl border border-line bg-surface">
        <div className="flex flex-wrap items-center justify-between gap-3 border-b border-line px-4 py-3">
          <div className="flex items-baseline gap-2">
            <h2 className="text-[13.5px] font-medium">Pages</h2>
            <span className="text-[12px] text-muted">
              {selectedSite ? selectedSite.name : "Loading sites…"}
            </span>
          </div>
          <div className="flex items-center gap-2">
            <label className="flex items-center gap-2">
              <span className="sr-only">Filter pages by state</span>
              <select
                value={filter}
                onChange={(event) => setFilter(event.target.value)}
                className="rounded-lg border border-line bg-surface px-2 py-1.5 text-[12px] text-ink outline-none focus:border-accent"
              >
                {FILTERS.map((option) => (
                  <option key={option.value} value={option.value}>
                    {option.label}
                  </option>
                ))}
              </select>
            </label>
            <button
              type="button"
              onClick={reload}
              aria-label="Reload pages"
              className="rounded-lg border border-line bg-surface p-2 text-muted transition hover:text-ink"
            >
              <RefreshCw className="size-3.5" aria-hidden />
            </button>
            <button
              type="button"
              onClick={openCreate}
              className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
            >
              <Plus className="size-3.5" aria-hidden />
              New page
            </button>
          </div>
        </div>

        {error ? (
          <div className="flex flex-col items-center gap-3 px-6 py-10 text-center">
            <p className="text-[12.5px] text-accent-strong">{error}</p>
            <button
              type="button"
              onClick={reload}
              className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
            >
              Try again
            </button>
          </div>
        ) : pages === null ? (
          <LoadingTable columns={5} />
        ) : pages.length === 0 ? (
          <EmptyState
            title={filter ? "No pages in this state" : "This site has no pages yet"}
            hint={
              filter
                ? "Clear the filter to see every page of the site."
                : "A page is one addressable piece of content: its revisions are kept, and the one you publish is what visitors see."
            }
            action={
              filter ? undefined : (
                <button
                  type="button"
                  onClick={openCreate}
                  className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
                >
                  <Plus className="size-3.5" aria-hidden />
                  New page
                </button>
              )
            }
          />
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full border-collapse text-left text-[13px]">
              <thead>
                <tr className="bg-canvas/60 text-[11px] font-medium tracking-wide text-muted uppercase">
                  <th scope="col" className="px-4 py-2.5">
                    Page
                  </th>
                  <th scope="col" className="px-4 py-2.5">
                    Type
                  </th>
                  <th scope="col" className="px-4 py-2.5">
                    State
                  </th>
                  <th scope="col" className="px-4 py-2.5">
                    Updated
                  </th>
                  <th scope="col" className="px-4 py-2.5">
                    Actions
                  </th>
                </tr>
              </thead>
              <tbody>
                {pages.map((page) => (
                  <tr key={page.id} className="border-t border-line transition hover:bg-canvas/60">
                    <td className="px-4 py-3.5">
                      <span className="flex min-w-0 flex-col leading-tight">
                        <span className="truncate font-medium">{pageTitle(page)}</span>
                        <span className="truncate text-[11.5px] text-muted">
                          /{page.slug} · {revisionNote(page)}
                        </span>
                      </span>
                    </td>
                    <td className="px-4 py-3.5 text-muted">{page.page_type}</td>
                    <td className="px-4 py-3.5">
                      <StatusBadge status={page.status} />
                    </td>
                    <td className="px-4 py-3.5 text-muted">{formatTimestamp(page.updated_at)}</td>
                    <td className="px-4 py-3.5">
                      <span className="flex items-center gap-2">
                        <button
                          type="button"
                          onClick={() => openEdit(page)}
                          aria-label={`Edit ${pageTitle(page)}`}
                          className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1 text-[12px] transition hover:bg-canvas"
                        >
                          <Pencil className="size-3" aria-hidden />
                          Edit
                        </button>
                        <button
                          type="button"
                          onClick={() => void publish(page)}
                          disabled={!page.draft || publishing === page.id}
                          aria-label={`Publish ${pageTitle(page)}`}
                          className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1 text-[12px] transition hover:bg-canvas disabled:opacity-50 disabled:hover:bg-transparent"
                        >
                          <Rocket className="size-3" aria-hidden />
                          Publish
                        </button>
                      </span>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </div>
  );
}
