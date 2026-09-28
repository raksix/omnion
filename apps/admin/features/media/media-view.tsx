"use client";

/**
 * Media browser: the file manager of the selected site (docs/requests/REQ-010, slice 1).
 *
 * The v0 screen was a flat table of files with an upload button. This is the file system: a
 * folder tree on the left, a breadcrumb and a toolbar on the right, and the files of one folder
 * under it in either a list or a grid. A selection turns on the bulk bar, and a delete moves files
 * to the trash instead of destroying them — the trash screen brings them back.
 *
 * Every control here is wired to a real route. There is no disabled button and no placeholder:
 * a view the API cannot answer yet is simply not on this screen.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  AlertTriangle,
  ChevronRight,
  Copy,
  Folder,
  FolderPlus,
  Grid2x2,
  Info,
  LayoutList,
  ListFilter,
  Pencil,
  Plus,
  RefreshCw,
  Search,
  Tag,
  Trash2,
  Upload,
  X,
} from "lucide-react";
import Link from "next/link";
import { useRouter, useSearchParams } from "next/navigation";

import { EmptyState } from "@/components/empty-state";
import { ScanBadge, fileIcon } from "@/features/media/media-shared";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  createMediaFolder,
  deleteMediaFolder,
  fetchMediaFiles,
  fetchMediaFolders,
  mediaBulkAction,
  mediaRawUrl,
  moveMediaFolder,
  trashMediaFile,
  updateMediaFile,
  uploadMedia,
} from "@/lib/api";
import { formatBytes, formatTimestamp } from "@/lib/format";
import { useSites } from "@/lib/sites";
import type { MediaFile, MediaFilePage, MediaFolder, MediaFilters } from "@/lib/types";

/** How the content area is laid out. */
type ViewMode = "list" | "grid";

/** Sort keys, with the label each one shows. */
const SORTS: { value: string; label: string }[] = [
  { value: "newest", label: "Newest first" },
  { value: "oldest", label: "Oldest first" },
  { value: "largest", label: "Largest first" },
  { value: "smallest", label: "Smallest first" },
  { value: "name", label: "Name" },
  { value: "modified", label: "Recently modified" },
];

/** Kind filter, with the label each one shows. */
const KINDS: { value: string; label: string }[] = [
  { value: "", label: "All kinds" },
  { value: "image", label: "Images" },
  { value: "video", label: "Video" },
  { value: "audio", label: "Audio" },
  { value: "application/pdf", label: "PDF" },
  { value: "text", label: "Text" },
];

/** Scan-state filter, with the label each one shows. */
const SCAN_STATES: { value: string; label: string }[] = [
  { value: "", label: "Any scan state" },
  { value: "pending", label: "Pending scan" },
  { value: "clean", label: "Clean" },
  { value: "flagged", label: "Flagged" },
  { value: "error", label: "Scan error" },
];

/** How many files a bulk bar will carry before it refuses to do more in one call. */
const MAX_BULK = 500;

/** The media browser: tree, breadcrumb, toolbar, content area and the bulk bar. */
export function MediaView() {
  const { selectedSite, status: sitesStatus } = useSites();
  const router = useRouter();
  const searchParams = useSearchParams();

  const [folders, setFolders] = useState<MediaFolder[] | null>(null);
  const [page, setPage] = useState<MediaFilePage | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // Toolbar state. `folderId` is the deep link (`/media?folder=…`), so a folder is shareable.
  const folderId = searchParams.get("folder");
  const [view, setView] = useState<ViewMode>("list");
  const [sort, setSort] = useState("newest");
  const [search, setSearch] = useState("");
  const [kind, setKind] = useState("");
  const [scanStatus, setScanStatus] = useState("");
  const [hasVersions, setHasVersions] = useState(false);
  const [recursive, setRecursive] = useState(true);
  const [showFilters, setShowFilters] = useState(false);
  const [selection, setSelection] = useState<string[]>([]);
  const [newFolderName, setNewFolderName] = useState("");
  const [renaming, setRenaming] = useState<string | null>(null);
  const [renameValue, setRenameValue] = useState("");

  const fileInput = useRef<HTMLInputElement>(null);
  const reloadToken = useRef(0);

  const siteId = selectedSite?.id ?? null;

  const filters: MediaFilters = useMemo(
    () => ({
      folder_id: folderId,
      recursive: folderId ? recursive : false,
      search: search.trim() || undefined,
      kind: kind || undefined,
      scan_status: scanStatus || undefined,
      has_versions: hasVersions || undefined,
      sort,
      limit: 200,
    }),
    [folderId, recursive, search, kind, scanStatus, hasVersions, sort],
  );

  const reload = useCallback(() => {
    reloadToken.current += 1;
  }, []);

  // The tree.
  useEffect(() => {
    if (!siteId) {
      setFolders(null);
      return;
    }
    let cancelled = false;
    fetchMediaFolders(siteId)
      .then((tree) => {
        if (!cancelled) {
          setFolders(tree.folders);
        }
      })
      .catch((cause: unknown) => {
        if (!cancelled) {
          setError(cause instanceof ApiError ? cause.message : "The folder tree could not be loaded.");
        }
      });
    return () => {
      cancelled = true;
    };
  }, [siteId, reloadToken.current]);

  // The listing.
  useEffect(() => {
    if (!siteId) {
      setPage(null);
      return;
    }
    let cancelled = false;
    setPage(null);
    setError(null);

    const handle = window.setTimeout(() => {
      fetchMediaFiles(siteId, filters)
        .then((answer) => {
          if (!cancelled) {
            setPage(answer);
          }
        })
        .catch((cause: unknown) => {
          if (cancelled) {
            return;
          }
          setError(
            cause instanceof ApiError ? cause.message : "This folder could not be listed.",
          );
        });
    }, search ? 250 : 0);

    return () => {
      cancelled = true;
      window.clearTimeout(handle);
    };
    // `reloadToken.current` is a ref, so it is read rather than watched; `reload` bumps it.
  }, [siteId, filters, reloadToken.current]);

  const activeFilters = [kind, scanStatus, hasVersions ? "versions" : "", search.trim()].filter(
    Boolean,
  ).length;

  const openFolder = useCallback(
    (id: string | null) => {
      setSelection([]);
      router.push(id ? `/media?folder=${encodeURIComponent(id)}` : "/media");
    },
    [router],
  );

  const currentFolder = folders?.find((folder) => folder.id === folderId) ?? null;

  const handleUpload = async (files: FileList | null) => {
    if (!siteId || !files || files.length === 0) {
      return;
    }
    setBusy(true);
    setError(null);
    setNotice(null);

    let uploaded = 0;
    for (const file of Array.from(files)) {
      try {
        await uploadMedia(siteId, file);
        uploaded += 1;
      } catch (cause) {
        setError(
          cause instanceof ApiError ? `${file.name}: ${cause.message}` : `${file.name} failed.`,
        );
        break;
      }
    }

    setBusy(false);
    if (fileInput.current) {
      fileInput.current.value = "";
    }
    if (uploaded > 0) {
      setNotice(uploaded === 1 ? "File uploaded." : `${uploaded} files uploaded.`);
      reload();
    }
  };

  const handleNewFolder = async () => {
    if (!siteId || !newFolderName.trim()) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await createMediaFolder(siteId, newFolderName.trim(), folderId ?? undefined);
      setNewFolderName("");
      setNotice(`Folder “${newFolderName.trim()}” created.`);
      reload();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The folder could not be created.");
    } finally {
      setBusy(false);
    }
  };

  const handleRenameFolder = async (folder: MediaFolder) => {
    if (!siteId || !renameValue.trim() || renaming !== folder.id) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await moveMediaFolder(folder.id, { name: renameValue.trim() });
      setRenaming(null);
      setNotice(`Folder renamed.`);
      reload();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The folder could not be renamed.");
    } finally {
      setBusy(false);
    }
  };

  const handleDeleteFolder = async (folder: MediaFolder) => {
    if (!siteId) {
      return;
    }
    // The confirmation says exactly what the API will refuse, so the two can never disagree.
    if (
      !window.confirm(
        `Delete the empty folder “${folder.name}”?\n\n` +
          "A folder that still holds files or subfolders is refused — delete or move them first.",
      )
    ) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await deleteMediaFolder(folder.id);
      if (folderId === folder.id) {
        openFolder(folder.parent_id);
      }
      setNotice(`Folder “${folder.name}” deleted.`);
      reload();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The folder could not be deleted.");
    } finally {
      setBusy(false);
    }
  };

  const toggleSelected = (id: string) => {
    setSelection((current) =>
      current.includes(id) ? current.filter((entry) => entry !== id) : [...current, id],
    );
  };

  const handleBulk = async (action: "move" | "tag" | "delete") => {
    if (!siteId || selection.length === 0) {
      return;
    }
    const label =
      selection.length === 1 ? "this file" : `${selection.length} files`;
    if (action === "delete") {
      // A delete is a trash, not a purge, and the confirmation says so — the count is named
      // because a bulk bar acts on files the operator may not have looked at individually.
      if (!window.confirm(`Move ${label} to the trash?\n\nNothing is deleted yet — the trash screen can bring them back.`)) {
        return;
      }
    }
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      let result;
      if (action === "move") {
        result = await mediaBulkAction(siteId, "move", selection, {
          folder_id: folderId ?? null,
        });
      } else if (action === "tag") {
        const tag = window.prompt("Tag to add to the selection (comma separated):");
        if (!tag) {
          setBusy(false);
          return;
        }
        result = await mediaBulkAction(siteId, "tag", selection, {
          tags: tag.split(",").map((entry) => entry.trim()),
        });
      } else {
        result = await mediaBulkAction(siteId, "delete", selection);
      }
      setNotice(
        result.failures.length > 0
          ? `${result.changed} of ${result.requested} files changed. ${result.failures.length} failed: ${result.failures[0]?.message ?? ""}`
          : `${result.changed} ${result.changed === 1 ? "file" : "files"} updated.`,
      );
      setSelection([]);
      reload();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The selection could not be updated.");
    } finally {
      setBusy(false);
    }
  };

  const handleTrashOne = async (file: MediaFile) => {
    if (!siteId) {
      return;
    }
    if (!window.confirm(`Move “${file.filename}” to the trash?`)) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await trashMediaFile(file.id);
      setNotice(`“${file.filename}” moved to the trash.`);
      reload();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The file could not be trashed.");
    } finally {
      setBusy(false);
    }
  };

  const handleRenameFile = async (file: MediaFile) => {
    const name = window.prompt("New file name:", file.filename);
    if (!siteId || !name || name === file.filename) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await updateMediaFile(file.id, { filename: name });
      setNotice("File renamed.");
      reload();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The file could not be renamed.");
    } finally {
      setBusy(false);
    }
  };

  // Keyboard: the shortcuts the browser spec names, only when nothing else is being typed in.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target?.tagName === "INPUT" ||
        target?.tagName === "TEXTAREA" ||
        target?.isContentEditable === true;
      if (typing || event.metaKey || event.ctrlKey || event.altKey) {
        return;
      }
      if (event.key === "n") {
        event.preventDefault();
        document.getElementById("media-new-folder")?.focus();
      } else if (event.key === "u") {
        event.preventDefault();
        fileInput.current?.click();
      } else if (event.key === "Escape") {
        setSelection([]);
        setShowFilters(false);
        setRenaming(null);
      } else if ((event.key === "a" || event.key === "A") && event.shiftKey) {
        // Shift+A selects the page, so ⌘A (the platform select-all) stays available to the
        // browser and the palette keeps its own ⌘K.
        const ids = page?.files.map((file) => file.id) ?? [];
        setSelection(ids.slice(0, MAX_BULK));
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [page]);

  if (sitesStatus === "error") {
    return (
      <div className="rounded-xl border border-line bg-surface">
        <EmptyState
          title="The site list could not be loaded"
          hint="The media library follows the selected site, so it needs the sites first."
        />
      </div>
    );
  }

  if (sitesStatus === "ready" && !selectedSite) {
    return (
      <div className="rounded-xl border border-line bg-surface">
        <EmptyState
          title="No sites yet"
          hint="A library belongs to a site. Create the first one and it appears here."
        />
      </div>
    );
  }

  const files = page?.files ?? [];
  const allSelected = files.length > 0 && selection.length === files.length;

  return (
    <div className="flex flex-col gap-3 lg:flex-row">
      <aside
        aria-label="Folders"
        className="lg:w-64 lg:shrink-0 rounded-xl border border-line bg-surface"
      >
        <div className="flex items-center justify-between border-b border-line px-3 py-2.5">
          <h2 className="text-[13px] font-medium">Folders</h2>
          <button
            type="button"
            aria-label="Create a folder in the current folder"
            onClick={() => {
              openFolder(folderId);
              document.getElementById("media-new-folder")?.focus();
            }}
            className="rounded-lg border border-line p-1.5 text-muted transition hover:text-ink"
          >
            <FolderPlus className="size-3.5" aria-hidden />
          </button>
        </div>

        <div className="p-2">
          <label htmlFor="media-new-folder" className="sr-only">
            New folder name
          </label>
          <div className="flex gap-1.5">
            <input
              id="media-new-folder"
              value={newFolderName}
              onChange={(event) => setNewFolderName(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter") {
                  void handleNewFolder();
                }
              }}
              placeholder="New folder"
              className="w-full rounded-lg border border-line bg-canvas px-2 py-1.5 text-[12.5px] outline-none focus:border-accent"
            />
            <button
              type="button"
              onClick={() => void handleNewFolder()}
              disabled={busy || !newFolderName.trim()}
              aria-label="Create folder"
              className="rounded-lg bg-accent px-2 py-1.5 text-white transition hover:bg-accent-strong disabled:cursor-not-allowed disabled:bg-quiet-soft"
            >
              <Plus className="size-3.5" aria-hidden />
            </button>
          </div>
        </div>

        {folders === null ? (
          <div className="px-3 pb-3" aria-busy="true">
            <div className="space-y-1.5">
              {Array.from({ length: 4 }, (_, index) => (
                <div key={index} className="h-4 w-full animate-pulse rounded bg-quiet-soft" />
              ))}
            </div>
          </div>
        ) : (
          <ul className="max-h-[60vh] overflow-y-auto px-1 pb-2">
            {folders.map((folder) => {
              const isCurrent = folder.id === folderId;
              const isRenaming = renaming === folder.id;
              return (
                <li key={folder.id} className="group">
                  <div
                    className={`flex items-center gap-1 rounded-lg pr-1 text-[12.5px] transition ${
                      isCurrent ? "bg-accent-soft text-accent-strong" : "hover:bg-canvas"
                    }`}
                    style={{ paddingLeft: `${8 + folder.depth * 12}px` }}
                  >
                    <button
                      type="button"
                      onClick={() => openFolder(folder.id)}
                      aria-current={isCurrent ? "page" : undefined}
                      className="flex min-w-0 flex-1 items-center gap-1.5 py-1.5 text-left"
                    >
                      <Folder className="size-3.5 shrink-0" aria-hidden />
                      <span className="truncate">{folder.name}</span>
                      <span className="ml-auto shrink-0 text-[11px] text-muted">
                        {folder.file_count}
                      </span>
                    </button>
                    {!folder.is_root ? (
                      <span className="flex shrink-0 items-center opacity-0 transition group-hover:opacity-100 focus-within:opacity-100">
                        <button
                          type="button"
                          aria-label={`Rename ${folder.name}`}
                          onClick={() => {
                            setRenaming(folder.id);
                            setRenameValue(folder.name);
                          }}
                          className="rounded p-1 text-muted transition hover:text-ink"
                        >
                          <Pencil className="size-3" aria-hidden />
                        </button>
                        <button
                          type="button"
                          aria-label={`Delete ${folder.name}`}
                          onClick={() => void handleDeleteFolder(folder)}
                          className="rounded p-1 text-muted transition hover:text-accent-strong"
                        >
                          <Trash2 className="size-3" aria-hidden />
                        </button>
                      </span>
                    ) : null}
                  </div>
                  {isRenaming ? (
                    <div className="flex gap-1 px-2 pb-1.5" style={{ paddingLeft: `${8 + folder.depth * 12}px` }}>
                      <label htmlFor={`rename-${folder.id}`} className="sr-only">
                        New name for {folder.name}
                      </label>
                      <input
                        id={`rename-${folder.id}`}
                        value={renameValue}
                        autoFocus
                        onChange={(event) => setRenameValue(event.target.value)}
                        onKeyDown={(event) => {
                          if (event.key === "Enter") {
                            void handleRenameFolder(folder);
                          }
                          if (event.key === "Escape") {
                            setRenaming(null);
                          }
                        }}
                        className="w-full rounded border border-line bg-canvas px-1.5 py-1 text-[12px] outline-none focus:border-accent"
                      />
                      <button
                        type="button"
                        onClick={() => void handleRenameFolder(folder)}
                        className="rounded border border-line px-1.5 text-[11.5px] transition hover:bg-canvas"
                      >
                        Save
                      </button>
                    </div>
                  ) : null}
                </li>
              );
            })}
          </ul>
        )}
      </aside>

      <section className="min-w-0 flex-1 overflow-hidden rounded-xl border border-line bg-surface">
        <div className="flex flex-wrap items-center justify-between gap-2 border-b border-line px-4 py-3">
          <nav aria-label="Breadcrumb" className="flex min-w-0 items-center gap-1 text-[12.5px]">
            <button
              type="button"
              onClick={() => openFolder(null)}
              className={`transition hover:text-ink ${currentFolder ? "text-muted" : "font-medium"}`}
            >
              Media
            </button>
            {(page?.breadcrumb ?? []).map((crumb) => (
              <span key={crumb.id} className="flex min-w-0 items-center gap-1">
                <ChevronRight className="size-3 shrink-0 text-muted" aria-hidden />
                <button
                  type="button"
                  onClick={() => openFolder(crumb.id)}
                  className={`truncate transition hover:text-ink ${
                    crumb.id === folderId ? "font-medium" : "text-muted"
                  }`}
                >
                  {crumb.name}
                </button>
              </span>
            ))}
          </nav>

          <div className="flex items-center gap-1.5">
            <label htmlFor="media-search" className="sr-only">
              Search files by name
            </label>
            <div className="relative">
              <Search
                className="pointer-events-none absolute left-2 top-1/2 size-3.5 -translate-y-1/2 text-muted"
                aria-hidden
              />
              <input
                id="media-search"
                value={search}
                onChange={(event) => setSearch(event.target.value)}
                placeholder="Search files"
                className="w-40 rounded-lg border border-line bg-canvas py-1.5 pl-7 pr-2 text-[12.5px] outline-none focus:border-accent"
              />
            </div>

            <button
              type="button"
              onClick={() => setShowFilters((value) => !value)}
              aria-expanded={showFilters}
              aria-label="Filters"
              className={`rounded-lg border border-line p-2 transition hover:text-ink ${
                activeFilters > 0 ? "text-accent-strong" : "text-muted"
              }`}
            >
              <ListFilter className="size-3.5" aria-hidden />
            </button>

            <label htmlFor="media-sort" className="sr-only">
              Sort
            </label>
            <select
              id="media-sort"
              value={sort}
              onChange={(event) => setSort(event.target.value)}
              className="rounded-lg border border-line bg-surface px-2 py-1.5 text-[12.5px]"
            >
              {SORTS.map((entry) => (
                <option key={entry.value} value={entry.value}>
                  {entry.label}
                </option>
              ))}
            </select>

            <div className="flex items-center rounded-lg border border-line">
              <button
                type="button"
                onClick={() => setView("list")}
                aria-pressed={view === "list"}
                aria-label="List view"
                className={`rounded-l-lg px-2 py-1.5 transition ${
                  view === "list" ? "bg-accent-soft text-accent-strong" : "text-muted"
                }`}
              >
                <LayoutList className="size-3.5" aria-hidden />
              </button>
              <button
                type="button"
                onClick={() => setView("grid")}
                aria-pressed={view === "grid"}
                aria-label="Grid view"
                className={`rounded-r-lg px-2 py-1.5 transition ${
                  view === "grid" ? "bg-accent-soft text-accent-strong" : "text-muted"
                }`}
              >
                <Grid2x2 className="size-3.5" aria-hidden />
              </button>
            </div>

            <input
              ref={fileInput}
              type="file"
              multiple
              className="hidden"
              onChange={(event) => void handleUpload(event.target.files)}
            />
            <button
              type="button"
              disabled={!siteId || busy}
              onClick={() => fileInput.current?.click()}
              className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:cursor-not-allowed disabled:bg-quiet-soft disabled:text-muted"
            >
              <Upload className="size-3.5" aria-hidden />
              {busy ? "Working…" : "Upload"}
            </button>
            <Link
              href="/media/trash"
              className="rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] text-muted transition hover:text-ink"
            >
              Trash
            </Link>
            <button
              type="button"
              onClick={reload}
              aria-label="Reload the media library"
              className="rounded-lg border border-line bg-surface p-2 text-muted transition hover:text-ink"
            >
              <RefreshCw className="size-3.5" aria-hidden />
            </button>
          </div>
        </div>

        {showFilters ? (
          <div className="flex flex-wrap items-end gap-3 border-b border-line bg-canvas/40 px-4 py-3">
            <div>
              <label
                htmlFor="media-kind"
                className="mb-1 block text-[11.5px] font-medium text-muted"
              >
                Kind
              </label>
              <select
                id="media-kind"
                value={kind}
                onChange={(event) => setKind(event.target.value)}
                className="rounded-lg border border-line bg-surface px-2 py-1.5 text-[12.5px]"
              >
                {KINDS.map((entry) => (
                  <option key={entry.value} value={entry.value}>
                    {entry.label}
                  </option>
                ))}
              </select>
            </div>
            <div>
              <label
                htmlFor="media-scan"
                className="mb-1 block text-[11.5px] font-medium text-muted"
              >
                Scan state
              </label>
              <select
                id="media-scan"
                value={scanStatus}
                onChange={(event) => setScanStatus(event.target.value)}
                className="rounded-lg border border-line bg-surface px-2 py-1.5 text-[12.5px]"
              >
                {SCAN_STATES.map((entry) => (
                  <option key={entry.value} value={entry.value}>
                    {entry.label}
                  </option>
                ))}
              </select>
            </div>
            <label className="flex items-center gap-1.5 text-[12.5px]">
              <input
                type="checkbox"
                checked={hasVersions}
                onChange={(event) => setHasVersions(event.target.checked)}
              />
              Has versions
            </label>
            {folderId ? (
              <label className="flex items-center gap-1.5 text-[12.5px]">
                <input
                  type="checkbox"
                  checked={recursive}
                  onChange={(event) => setRecursive(event.target.checked)}
                />
                Include subfolders
              </label>
            ) : null}
            {activeFilters > 0 ? (
              <button
                type="button"
                onClick={() => {
                  setSearch("");
                  setKind("");
                  setScanStatus("");
                  setHasVersions(false);
                }}
                className="rounded-lg border border-line px-2.5 py-1.5 text-[12px] transition hover:bg-surface"
              >
                Clear filters
              </button>
            ) : null}
          </div>
        ) : null}

        {notice ? (
          <p className="border-b border-line bg-canvas/60 px-4 py-2 text-[12px] text-muted">
            {notice}
          </p>
        ) : null}
        {error ? (
          <div
            role="alert"
            className="flex flex-col items-center gap-3 border-b border-line px-6 py-6 text-center"
          >
            <AlertTriangle className="size-5 text-accent-strong" aria-hidden />
            <p className="text-[12.5px] text-accent-strong">{error}</p>
            <button
              type="button"
              onClick={reload}
              className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
            >
              Try again
            </button>
          </div>
        ) : null}

        {page === null && !error ? (
          <LoadingTable columns={6} />
        ) : files.length === 0 ? (
          <EmptyState
            title={activeFilters > 0 ? "No files match these filters" : "This folder is empty"}
            hint={
              activeFilters > 0
                ? "Clear the filters to see the whole folder."
                : "Upload a file, or create a folder to organise what is already here."
            }
            action={
              activeFilters > 0 ? (
                <button
                  type="button"
                  onClick={() => {
                    setSearch("");
                    setKind("");
                    setScanStatus("");
                    setHasVersions(false);
                  }}
                  className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
                >
                  Clear filters
                </button>
              ) : (
                <div className="flex gap-2">
                  <button
                    type="button"
                    onClick={() => fileInput.current?.click()}
                    className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
                  >
                    Upload a file
                  </button>
                  <button
                    type="button"
                    onClick={() => document.getElementById("media-new-folder")?.focus()}
                    className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
                  >
                    New folder
                  </button>
                </div>
              )
            }
          />
        ) : view === "list" ? (
          <div className="overflow-x-auto">
            <table className="w-full border-collapse text-left text-[13px]">
              <thead>
                <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                  <th scope="col" className="w-8 px-3 py-2">
                    <label className="sr-only" htmlFor="media-select-all">
                      Select every file on this page
                    </label>
                    <input
                      id="media-select-all"
                      type="checkbox"
                      checked={allSelected}
                      onChange={(event) =>
                        setSelection(event.target.checked ? files.map((file) => file.id) : [])
                      }
                    />
                  </th>
                  <th scope="col" className="px-3 py-2 font-medium">
                    Name
                  </th>
                  <th scope="col" className="px-3 py-2 font-medium">
                    Kind
                  </th>
                  <th scope="col" className="px-3 py-2 font-medium">
                    Size
                  </th>
                  <th scope="col" className="px-3 py-2 font-medium">
                    Uploaded
                  </th>
                  <th scope="col" className="px-3 py-2 font-medium">
                    Scan
                  </th>
                  <th scope="col" className="px-3 py-2 font-medium">
                    Actions
                  </th>
                </tr>
              </thead>
              <tbody>
                {files.map((file) => {
                  const Icon = fileIcon(file);
                  const selected = selection.includes(file.id);
                  return (
                    <tr
                      key={file.id}
                      className={`border-t border-line transition hover:bg-canvas/60 ${
                        selected ? "bg-accent-soft/40" : ""
                      }`}
                    >
                      <td className="px-3 py-2.5">
                        <label className="sr-only" htmlFor={`select-${file.id}`}>
                          Select {file.filename}
                        </label>
                        <input
                          id={`select-${file.id}`}
                          type="checkbox"
                          checked={selected}
                          onChange={() => toggleSelected(file.id)}
                        />
                      </td>
                      <td className="px-3 py-2.5">
                        <div className="flex min-w-0 items-center gap-2">
                          {file.kind === "image" ? (
                            // A real thumbnail from the file's own read path, with a file-type icon
                            // behind it so a failed load degrades to something readable rather
                            // than to a broken frame.
                            <span className="relative flex size-8 shrink-0 items-center justify-center overflow-hidden rounded border border-line bg-canvas">
                              <Icon className="size-3.5 text-muted" aria-hidden />
                              <img
                                src={mediaRawUrl(file.id)}
                                alt=""
                                loading="lazy"
                                className="absolute inset-0 size-full object-cover"
                                onError={(event) => {
                                  event.currentTarget.style.display = "none";
                                }}
                              />
                            </span>
                          ) : (
                            <span className="flex size-8 shrink-0 items-center justify-center rounded border border-line bg-canvas">
                              <Icon className="size-3.5 text-muted" aria-hidden />
                            </span>
                          )}
                          <div className="min-w-0">
                            <Link
                              href={`/media/files/${file.id}`}
                              className="block truncate font-medium underline-offset-2 hover:underline"
                            >
                              {file.filename}
                            </Link>
                            {file.tags.length > 0 ? (
                              <p className="truncate text-[11.5px] text-muted">
                                {file.tags.join(", ")}
                              </p>
                            ) : null}
                          </div>
                        </div>
                      </td>
                      <td className="px-3 py-2.5 text-muted">{file.kind}</td>
                      <td className="px-3 py-2.5 text-muted">{formatBytes(file.size_bytes)}</td>
                      <td className="px-3 py-2.5 text-muted">
                        {formatTimestamp(file.created_at)}
                      </td>
                      <td className="px-3 py-2.5">
                        <ScanBadge status={file.scan_status} />
                      </td>
                      <td className="px-3 py-2.5">
                        <div className="flex items-center gap-1">
                          <Link
                            href={`/media/files/${file.id}`}
                            aria-label={`Open the details of ${file.filename}`}
                            className="rounded p-1.5 text-muted transition hover:text-ink"
                          >
                            <Info className="size-3.5" aria-hidden />
                          </Link>
                          <a
                            href={mediaRawUrl(file.id)}
                            aria-label={`Download ${file.filename}`}
                            className="rounded p-1.5 text-muted transition hover:text-ink"
                          >
                            <Copy className="size-3.5" aria-hidden />
                          </a>
                          <button
                            type="button"
                            onClick={() => void handleRenameFile(file)}
                            aria-label={`Rename ${file.filename}`}
                            className="rounded p-1.5 text-muted transition hover:text-ink"
                          >
                            <Pencil className="size-3.5" aria-hidden />
                          </button>
                          <button
                            type="button"
                            onClick={() => void handleTrashOne(file)}
                            aria-label={`Move ${file.filename} to the trash`}
                            className="rounded p-1.5 text-muted transition hover:text-accent-strong"
                          >
                            <Trash2 className="size-3.5" aria-hidden />
                          </button>
                        </div>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        ) : (
          <ul className="grid grid-cols-2 gap-3 p-4 sm:grid-cols-3 lg:grid-cols-4">
            {files.map((file) => {
              const Icon = fileIcon(file);
              const selected = selection.includes(file.id);
              return (
                <li
                  key={file.id}
                  className={`overflow-hidden rounded-lg border transition ${
                    selected ? "border-accent" : "border-line hover:border-accent/40"
                  }`}
                >
                  <button
                    type="button"
                    onClick={() => toggleSelected(file.id)}
                    aria-pressed={selected}
                    className="block w-full text-left"
                  >
                    <span className="relative flex h-24 items-center justify-center bg-canvas">
                      <Icon className="size-6 text-muted" aria-hidden />
                      {file.kind === "image" ? (
                        <img
                          src={mediaRawUrl(file.id)}
                          alt=""
                          loading="lazy"
                          className="absolute inset-0 size-full object-cover"
                          onError={(event) => {
                            event.currentTarget.style.display = "none";
                          }}
                        />
                      ) : null}
                      <span className="absolute left-1.5 top-1.5">
                        <input
                          type="checkbox"
                          checked={selected}
                          readOnly
                          tabIndex={-1}
                          aria-label={`Select ${file.filename}`}
                        />
                      </span>
                    </span>
                    <span className="block px-2.5 py-2">
                      <span className="block truncate text-[12.5px] font-medium">
                        {file.filename}
                      </span>
                      <span className="mt-0.5 block text-[11.5px] text-muted">
                        {formatBytes(file.size_bytes)} · {file.kind}
                      </span>
                    </span>
                  </button>
                </li>
              );
            })}
          </ul>
        )}

        {page !== null && files.length > 0 ? (
          <div className="flex items-center justify-between border-t border-line px-4 py-2 text-[11.5px] text-muted">
            <span>
              Showing {files.length} of {page.total} {page.total === 1 ? "file" : "files"}
            </span>
            {page.has_more ? (
              <span className="text-muted">Refine the filters to see the rest.</span>
            ) : null}
          </div>
        ) : null}

        {selection.length > 0 ? (
          <div
            role="region"
            aria-label="Selection"
            className="sticky bottom-0 flex flex-wrap items-center gap-2 border-t border-line bg-surface px-4 py-2.5 shadow-[0_-4px_12px_rgba(0,0,0,0.05)]"
          >
            <span className="text-[12.5px] font-medium">
              {selection.length} selected
            </span>
            <button
              type="button"
              onClick={() => void handleBulk("move")}
              disabled={busy}
              className="rounded-lg border border-line px-2.5 py-1 text-[12px] transition hover:bg-canvas disabled:opacity-50"
            >
              Move here
            </button>
            <button
              type="button"
              onClick={() => void handleBulk("tag")}
              disabled={busy}
              className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1 text-[12px] transition hover:bg-canvas disabled:opacity-50"
            >
              <Tag className="size-3" aria-hidden />
              Tag
            </button>
            <button
              type="button"
              onClick={() => void handleBulk("delete")}
              disabled={busy}
              className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1 text-[12px] text-accent-strong transition hover:bg-canvas disabled:opacity-50"
            >
              <Trash2 className="size-3" aria-hidden />
              Delete
            </button>
            <button
              type="button"
              onClick={() => setSelection([])}
              className="ml-auto flex items-center gap-1.5 rounded-lg px-2.5 py-1 text-[12px] text-muted transition hover:text-ink"
            >
              <X className="size-3" aria-hidden />
              Clear
            </button>
          </div>
        ) : null}
      </section>
    </div>
  );
}
