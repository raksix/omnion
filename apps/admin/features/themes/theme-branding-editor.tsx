"use client";

/**
 * The branding section of the customize screen (REQ-062, criterion 9).
 *
 * The API half of this criterion landed in tick 59: `check_branding` refuses an oversize, an
 * out-of-range or a wrong-typed asset and names the field. What was missing was the panel half,
 * and the missing half is the reason the criterion could not be ticked — a refusal an operator
 * only sees after pressing Save is a refusal they read once and then guess at next time.
 *
 * Four decisions, each one because the obvious version loses information:
 *
 * 1. **The picker, not a text box.** A branding key is a media id, and a media id typed by hand
 *    is a value nobody can verify. The three inputs offer the site's own library, so the id is
 *    always a file that exists — and the previous value stays reachable, because a save that
 *    refused a logo must not have destroyed the one that worked.
 * 2. **The limits come from the server, shown beside the input.** `brandingLimits` is read from
 *    the same manifest, through the same `BrandingLimits::for_theme`, as the save-time check.
 *    A client-side copy would be a second implementation of a rule that already exists — and it
 *    would be wrong for every theme that tightens it, which is precisely the case where the
 *    operator needs the number.
 * 3. **One message per field, from `details.findings[].field`, never from the joined sentence.**
 *    The server already returns every finding tagged with its field. Splitting them here means an
 *    operator who fixed the logo and resubmits finds the favicon message already waiting, rather
 *    than discovering it in the next round trip.
 * 4. **A file the picker accepted is not a file that passed.** Selection stores the id and
 *    clears that field's messages — the measurement belongs to the save, because the panel has
 *    no geometry to check against (the media row carries size and type but not width/height).
 *    So the button is honest: it says what it did, not what it decided.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { AlertTriangle, Image as ImageIcon, Loader2, Upload, X } from "lucide-react";

import { ApiError, fetchMedia, mediaRawUrl, uploadMedia } from "@/lib/api";
import type { Media } from "@/lib/types";

/** The three keys the renderer reads, in the order the section shows them. */
const BRANDING_SLOTS = [
  {
    key: "logo",
    label: "Logo",
    hint: "Shown in the header on every page. Keep it wide and short.",
  },
  {
    key: "logoDark",
    label: "Logo (dark mode)",
    hint: "Optional. Falls back to the logo when a visitor is in dark mode.",
  },
  {
    key: "favicon",
    label: "Favicon",
    hint: "The small icon a browser shows in a tab. Usually square.",
  },
] as const;

export type BrandingLimits = {
  maxBytes: number;
  minPx: number;
  maxPx: number;
  contentTypes: string[];
};

/** One server finding, narrowed to what the panel renders. */
type BrandingFinding = { field?: string; message?: string };

/** Human byte count — the ceiling has to be readable without doing arithmetic. */
function formatBytes(bytes: number): string {
  if (bytes >= 1024 * 1024) {
    const mb = bytes / (1024 * 1024);
    return `${Number.isInteger(mb) ? mb : mb.toFixed(1)} MB`;
  }
  if (bytes >= 1024) return `${Math.round(bytes / 1024)} kB`;
  return `${bytes} bytes`;
}

/** `image/png` → `PNG`, so the type list reads as formats rather than as MIME strings. */
function shortType(contentType: string): string {
  const subtype = contentType.split("/")[1] ?? contentType;
  return subtype.replace(/^x-/, "").toUpperCase();
}

/**
 * Pull the per-field messages out of a save refusal.
 *
 * `details.findings` is the contract the route builds with `.with_details(json!({ findings }))`.
 * An older API that answers the same code without the array must not crash the section, so a
 * refusal with no findings falls back to the message itself on every field that has a value —
 * worse than a precise answer, but never a blank screen where an explanation belongs.
 */
export function findingsFrom(error: unknown): Record<string, string[]> {
  const out: Record<string, string[]> = {};
  if (!(error instanceof ApiError)) return out;
  if (error.code !== "theme_settings_branding_invalid") return out;

  const details = error.details as { findings?: BrandingFinding[] } | null;
  const findings = details?.findings;
  if (Array.isArray(findings)) {
    for (const finding of findings) {
      const field = finding.field;
      const message = finding.message;
      if (!field || !message) continue;
      (out[field] ??= []).push(message);
    }
    return out;
  }

  const single = error.message;
  for (const slot of BRANDING_SLOTS) out[slot.key] = [single];
  return out;
}

export function ThemeBrandingEditor({
  siteId,
  values,
  limits,
  messages,
  onChange,
  onClear,
}: {
  siteId: string;
  values: Record<string, unknown>;
  limits: BrandingLimits;
  /** Per-field messages from the last save refusal, keyed by branding key. */
  messages: Record<string, string[]>;
  onChange: (key: string, value: unknown) => void;
  /** Drop the stored value — the panel's own "no logo here" state, which the server reads as `null`. */
  onClear: (key: string) => void;
}) {
  const [media, setMedia] = useState<Media[] | null>(null);
  const [mediaError, setMediaError] = useState<string | null>(null);
  const [uploading, setUploading] = useState<string | null>(null);
  const [uploadError, setUploadError] = useState<{ slot: string; text: string } | null>(null);

  const load = useCallback(async () => {
    setMedia(null);
    setMediaError(null);
    try {
      const library = await fetchMedia(siteId);
      // Images only: a video is storable and is not a logo, and offering it in a logo picker
      // invites the 422 the criterion exists to prevent.
      setMedia(library.filter((item) => item.content_type.startsWith("image/")));
    } catch (caught) {
      setMediaError((caught as ApiError).message);
    }
  }, [siteId]);

  useEffect(() => {
    void load();
  }, [load]);

  const upload = useCallback(
    async (slot: string, file: File) => {
      setUploading(slot);
      setUploadError(null);
      try {
        const stored = await uploadMedia(siteId, file);
        onChange(slot, stored.id);
        // The uploaded file is not in the list the picker loaded from, so it is added rather
        // than waited for: an operator who just picked a file must see it selected.
        setMedia((current) => (current ? [stored, ...current] : [stored]));
      } catch (caught) {
        setUploadError({ slot, text: (caught as ApiError).message });
      } finally {
        setUploading(null);
      }
    },
    [onChange, siteId],
  );

  // The accept filter is derived from the server's own list, so a theme that declares fewer
  // types than the platform default cannot be offered a file the save will refuse.
  const accept = useMemo(
    () => (limits.contentTypes ?? []).join(","),
    [limits.contentTypes],
  );

  const limitLine = `${formatBytes(limits.maxBytes)} · ${limits.minPx}–${limits.maxPx} px · ${(
    limits.contentTypes ?? []
  )
    .map(shortType)
    .join(", ")}`;

  return (
    <div className="space-y-4" data-theme-branding-editor>
      <p className="text-xs text-muted" data-theme-branding-limits={limitLine}>
        Accepted: {limitLine}. A refusal names the field, so fix one and resubmit — the others
        are listed with it.
      </p>

      {mediaError ? (
        <p className="flex items-center gap-2 rounded-md border border-line bg-panel px-3 py-2 text-sm text-muted" data-theme-branding-library-error>
          <AlertTriangle className="h-4 w-4 shrink-0" aria-hidden />
          The media library could not be loaded ({mediaError}), so a file cannot be picked. Paste a
          media id below instead, or reload this page.
        </p>
      ) : null}

      {BRANDING_SLOTS.map((slot) => {
        const current = asOptionalString(values[slot.key]);
        const fieldMessages = messages[slot.key] ?? [];
        const inputId = `branding-${slot.key}`;
        return (
          <div key={slot.key} className="rounded-md border border-line p-3" data-theme-branding-slot={slot.key}>
            <div className="flex flex-wrap items-center justify-between gap-2">
              <label htmlFor={inputId} className="text-sm font-medium text-ink">
                {slot.label}
              </label>
              <div className="flex items-center gap-2">
                {current ? (
                  <>
                    <img
                      src={mediaRawUrl(current)}
                      alt=""
                      className="h-8 max-w-[120px] rounded border border-line bg-surface object-contain"
                      data-theme-branding-thumb={slot.key}
                    />
                    <button
                      type="button"
                      className="btn btn-ghost"
                      data-theme-branding-clear={slot.key}
                      onClick={() => onClear(slot.key)}
                      title={`Remove the ${slot.label.toLowerCase()}`}
                      aria-label={`Remove the ${slot.label.toLowerCase()}`}
                    >
                      <X className="h-4 w-4" aria-hidden />
                      Remove
                    </button>
                  </>
                ) : (
                  <span className="text-xs text-muted" data-theme-branding-empty={slot.key}>
                    Not set — the theme&apos;s own mark is used.
                  </span>
                )}
              </div>
            </div>
            <p className="mt-1 text-xs text-muted">{slot.hint}</p>

            <div className="mt-2 flex flex-wrap items-center gap-2">
              <input
                id={inputId}
                type="text"
                className="input min-w-0 flex-1"
                data-theme-branding-input={slot.key}
                value={current}
                placeholder="media id"
                aria-describedby={fieldMessages.length > 0 ? `${inputId}-error` : undefined}
                aria-invalid={fieldMessages.length > 0}
                onChange={(event) => onChange(slot.key, event.target.value)}
              />
              <label className="btn btn-ghost cursor-pointer" data-theme-branding-upload={slot.key}>
                {uploading === slot.key ? (
                  <Loader2 className="h-4 w-4 animate-spin" aria-hidden />
                ) : (
                  <Upload className="h-4 w-4" aria-hidden />
                )}
                {uploading === slot.key ? "Uploading" : "Upload"}
                <input
                  type="file"
                  className="sr-only"
                  accept={accept}
                  data-theme-branding-file={slot.key}
                  onChange={(event) => {
                    const file = event.target.files?.[0];
                    if (file) void upload(slot.key, file);
                    // Reset so picking the SAME file twice in a row fires a change event.
                    event.target.value = "";
                  }}
                />
              </label>
            </div>

            {uploadError?.slot === slot.key ? (
              <p className="mt-1 text-xs text-danger" role="alert" data-theme-branding-upload-error={slot.key}>
                {uploadError.text}
              </p>
            ) : null}

            {fieldMessages.length > 0 ? (
              <ul
                id={`${inputId}-error`}
                className="mt-1 space-y-0.5 text-xs text-danger"
                role="alert"
                data-theme-branding-error={slot.key}
              >
                {fieldMessages.map((message) => (
                  <li key={message}>{message}</li>
                ))}
              </ul>
            ) : null}

            {media && media.length > 0 ? (
              <details className="mt-2">
                <summary className="cursor-pointer text-xs text-muted" data-theme-branding-picker={slot.key}>
                  Pick from the library ({media.length})
                </summary>
                <ul className="mt-2 grid grid-cols-4 gap-2 sm:grid-cols-6">
                  {media.map((item) => (
                    <li key={item.id}>
                      <button
                        type="button"
                        className={`block w-full overflow-hidden rounded border ${
                          item.id === current ? "border-accent" : "border-line"
                        }`}
                        data-theme-branding-choice={item.id}
                        title={`${item.filename} · ${formatBytes(item.size_bytes)}`}
                        onClick={() => onChange(slot.key, item.id)}
                      >
                        <img
                          src={mediaRawUrl(item.id)}
                          alt={item.filename}
                          className="h-12 w-full bg-surface object-contain"
                          loading="lazy"
                        />
                      </button>
                    </li>
                  ))}
                </ul>
              </details>
            ) : null}
          </div>
        );
      })}

      {media && media.length === 0 ? (
        <p className="flex items-center gap-2 text-xs text-muted" data-theme-branding-empty-library>
          <ImageIcon className="h-4 w-4 shrink-0" aria-hidden />
          The library holds no images yet. Upload a logo above and it appears here as well as in
          the header.
        </p>
      ) : null}
    </div>
  );
}

/** A stored value as text; `null`, `undefined` and a non-string all read as "not set". */
function asOptionalString(value: unknown): string {
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  return "";
}