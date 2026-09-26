"use client";

/**
 * Media screen: the library of the site the switcher is on.
 *
 * Files are uploaded into the selected site, listed with a preview for the types a browser can
 * render, and removed together with their object. The screen mirrors the pages view — loading,
 * empty and error states — so the panel reads the same everywhere.
 */
import { useCallback, useEffect, useRef, useState } from "react";

import { RefreshCw, Trash2, Upload } from "lucide-react";
import { useSearchParams } from "next/navigation";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { ApiError, deleteMedia, fetchMedia, mediaRawUrl, uploadMedia } from "@/lib/api";
import { formatBytes, formatTimestamp } from "@/lib/format";
import { useSites } from "@/lib/sites";
import type { Media } from "@/lib/types";

/** Content types the list shows as a picture. */
function isImage(contentType: string): boolean {
  return contentType.startsWith("image/");
}

/** The media library. */
export function MediaView() {
  const { selectedSite, status: sitesStatus } = useSites();
  const searchParams = useSearchParams();
  const [media, setMedia] = useState<Media[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [reloadToken, setReloadToken] = useState(0);
  const [focusedId, setFocusedId] = useState<string | null>(null);
  const fileInput = useRef<HTMLInputElement>(null);

  const reload = useCallback(() => setReloadToken((token) => token + 1), []);

  // `/media?focus=<id>` — a search hit names the file it came from, so the library brings that row
  // into view and marks it instead of leaving the visitor to find it.
  const focusParam = searchParams.get("focus");
  useEffect(() => {
    if (!focusParam || !media || !media.some((item) => item.id === focusParam)) {
      return;
    }
    setFocusedId(focusParam);
    document.getElementById(`media-${focusParam}`)?.scrollIntoView({ block: "center" });
  }, [focusParam, media]);

  useEffect(() => {
    if (!selectedSite) {
      setMedia(null);
      setError(null);
      return;
    }

    let cancelled = false;
    setMedia(null);
    setError(null);
    fetchMedia(selectedSite.id)
      .then((rows) => {
        if (!cancelled) {
          setMedia(rows);
        }
      })
      .catch((cause: unknown) => {
        if (cancelled) {
          return;
        }
        setError(
          cause instanceof ApiError ? cause.message : "The media library could not be loaded.",
        );
      });

    return () => {
      cancelled = true;
    };
  }, [selectedSite, reloadToken]);

  const handleUpload = async (files: FileList | null) => {
    if (!selectedSite || !files || files.length === 0) {
      return;
    }

    setBusy(true);
    setError(null);
    setNotice(null);

    let uploaded = 0;
    for (const file of Array.from(files)) {
      try {
        await uploadMedia(selectedSite.id, file);
        uploaded += 1;
      } catch (cause) {
        setError(
          cause instanceof ApiError
            ? `${file.name}: ${cause.message}`
            : `${file.name} could not be uploaded.`,
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

  const handleDelete = async (item: Media) => {
    if (!window.confirm(`Remove ${item.filename} from the library?`)) {
      return;
    }

    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await deleteMedia(item.id);
      setNotice(`${item.filename} removed.`);
      reload();
    } catch (cause) {
      setError(
        cause instanceof ApiError ? cause.message : `${item.filename} could not be removed.`,
      );
    } finally {
      setBusy(false);
    }
  };

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
          hint="A library belongs to a site. Create the first one through POST /api/v1/sites and it appears here."
        />
      </div>
    );
  }

  return (
    <div className="overflow-hidden rounded-xl border border-line bg-surface">
      <div className="flex flex-wrap items-center justify-between gap-3 border-b border-line px-4 py-3">
        <div className="flex items-baseline gap-2">
          <h2 className="text-[13.5px] font-medium">Media</h2>
          <span className="text-[12px] text-muted">
            {selectedSite ? selectedSite.name : "Loading sites…"}
          </span>
        </div>
        <div className="flex items-center gap-2">
          <input
            ref={fileInput}
            type="file"
            multiple
            className="hidden"
            onChange={(event) => void handleUpload(event.target.files)}
          />
          <button
            type="button"
            disabled={!selectedSite || busy}
            onClick={() => fileInput.current?.click()}
            className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:cursor-not-allowed disabled:bg-quiet-soft disabled:text-muted"
          >
            <Upload className="size-3.5" aria-hidden />
            {busy ? "Working…" : "Upload file"}
          </button>
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

      {notice ? (
        <p className="border-b border-line bg-canvas/60 px-4 py-2 text-[12px] text-muted">
          {notice}
        </p>
      ) : null}

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
      ) : media === null ? (
        <LoadingTable columns={4} />
      ) : media.length === 0 ? (
        <EmptyState
          title="This site has no files yet"
          hint="Upload the first one — images, video, audio, PDFs and documents up to 25 MB."
        />
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full border-collapse text-left text-[13px]">
            <thead>
              <tr className="bg-canvas/60 text-[11px] font-medium tracking-wide text-muted uppercase">
                <th scope="col" className="px-4 py-2.5">
                  File
                </th>
                <th scope="col" className="px-4 py-2.5">
                  Type
                </th>
                <th scope="col" className="px-4 py-2.5">
                  Size
                </th>
                <th scope="col" className="px-4 py-2.5">
                  Uploaded
                </th>
                <th scope="col" className="px-4 py-2.5">
                  <span className="sr-only">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody>
              {media.map((item) => (
                <tr
                  key={item.id}
                  id={`media-${item.id}`}
                  className={`border-t border-line transition hover:bg-canvas/60 ${
                    focusedId === item.id ? "bg-accent-soft/60" : ""
                  }`}
                >
                  <td className="px-4 py-3.5">
                    <span className="flex min-w-0 items-center gap-3">
                      {isImage(item.content_type) ? (
                        // The library answers the bytes on the panel's own origin, through the
                        // session cookie, so a plain img element is enough here.
                        // eslint-disable-next-line @next/next/no-img-element
                        <img
                          src={mediaRawUrl(item.id)}
                          alt=""
                          className="size-10 shrink-0 rounded-md border border-line object-cover"
                        />
                      ) : (
                        <span className="flex size-10 shrink-0 items-center justify-center rounded-md border border-line bg-canvas text-[10px] text-muted uppercase">
                          {item.content_type.split("/")[1]?.slice(0, 4) ?? "file"}
                        </span>
                      )}
                      <span className="flex min-w-0 flex-col leading-tight">
                        <span className="truncate font-medium">{item.filename}</span>
                        <span className="truncate text-[11.5px] text-muted">
                          {item.public_path}
                        </span>
                      </span>
                    </span>
                  </td>
                  <td className="px-4 py-3.5 text-muted">{item.content_type}</td>
                  <td className="px-4 py-3.5 text-muted">{formatBytes(item.size_bytes)}</td>
                  <td className="px-4 py-3.5 text-muted">{formatTimestamp(item.created_at)}</td>
                  <td className="px-4 py-3.5 text-right">
                    <button
                      type="button"
                      disabled={busy}
                      onClick={() => void handleDelete(item)}
                      aria-label={`Remove ${item.filename}`}
                      className="rounded-lg border border-line p-2 text-muted transition hover:text-ink disabled:cursor-not-allowed disabled:opacity-50"
                    >
                      <Trash2 className="size-3.5" aria-hidden />
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
