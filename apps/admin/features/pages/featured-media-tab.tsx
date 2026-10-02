"use client";

/**
 * The page editor's media tab (REQ-064, slice 4d — "media reuse").
 *
 * A page has ONE featured image, and this screen is the whole of the decision: which file, what
 * it says, and where the crop centres. Six things it does that a form over five fields would
 * not, each one a way the obvious version lies:
 *
 * 1. **The preview is the renderer's payload, not a re-derivation.** It renders
 *    `body.render` — the exact object the public page carries — so the crop this screen shows is
 *    the crop visitors get. A preview built in the browser from the same five fields is a second
 *    implementation, and it is the one that goes stale.
 *
 * 2. **A trashed file is a third state, not "no image".** The chip reads "In the trash", the
 *    warning names the file and says the page still renders, and the crop controls stay ENABLED
 *    — because a restore brings the picture back with the crop the operator already set, and a
 *    screen that disabled them would have them re-set it after restoring. What the screen refuses
 *    is *choosing* a trashed file, which the picker does not offer at all.
 *
 * 3. **The alt is required, and the picker offers the file's own as a starting point.** The
 *    migration refuses a hero with a blank alt, so the field is marked required and the save
 *    button is disabled without it — the refusal happens before the round trip rather than as a
 *    red banner afterwards. The file's own `alt_text` is offered by an explicit *Use the file's
 *    alt* button and never copied on its own: the same photograph is the hero of three pages
 *    with three descriptions.
 *
 * 4. **The crop is a pad, not two number boxes.** Two numeric inputs asking for "0.62" is the
 *    platform's storage format leaking into the editor, and a typo is silent — the image is
 *    still there, just cropped on nothing. The pad drags a dot; the numbers follow it, and the
 *    numbers are still editable for the keyboard path. Arrow keys move it, because a pointer-only
 *    control is not usable without a mouse.
 *
 * 5. **The crop's two axes clear TOGETHER or not at all.** One *Clear crop* button, and it
 *    sends an explicit `null` pair — which is the only way the API can tell "clear this" from
 *    "leave it alone". The alternative, a checkbox per axis, is a control the schema would refuse
 *    anyway.
 *
 * 6. **Removing the image says what it does.** *Remove the image* clears the hero, the alt, the
 *    legend and the crop in one save, and the confirmation says all four — a button labelled
 *    "Remove" that quietly kept the crop is how an operator ends up re-attaching an image to a
 *    crop they chose for the last one.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  AlertTriangle,
  Check,
  Crosshair,
  ImageOff,
  Image as ImageIcon,
  Loader2,
  RotateCcw,
  Trash2,
  Upload,
} from "lucide-react";
import { useParams } from "next/navigation";

import { EmptyState } from "@/components/empty-state";
import {
  ApiError,
  fetchFeaturedCandidates,
  fetchFeaturedMedia,
  saveFeaturedMedia,
} from "@/lib/api";
import type {
  FeaturedCandidate,
  FeaturedMediaBody,
} from "@/lib/types";

/** How many candidate rows the picker asks for. The server clamps this too. */
const PICKER_PAGE = 60;

/** Longest alt the server accepts, mirrored so the counter is honest before the round trip. */
const MAX_ALT = 500;

/** Longest legend the server accepts. */
const MAX_LEGEND = 1000;

/** How a focal point reads, from the fraction the API stores. */
function percent(value: number | null): string {
  return value === null ? "—" : `${Math.round(value * 100)}%`;
}

/** The chip's colour, from the server's own availability word. */
function chipTone(label: string): string {
  if (label === "Available") {
    return "border-accent/40 bg-accent-soft text-accent-strong";
  }
  if (label === "In the trash" || label === "File missing") {
    return "border-caution/40 bg-caution-soft text-caution-strong";
  }
  return "border-line bg-canvas text-muted";
}

/** One row of the picker: a thumbnail, the name, and the reuse count. */
function CandidateRow({
  candidate,
  selected,
  onPick,
}: {
  candidate: FeaturedCandidate;
  selected: boolean;
  onPick: () => void;
}) {
  return (
    <li>
      <button
        type="button"
        data-featured-candidate={candidate.id}
        aria-pressed={selected}
        onClick={onPick}
        className={`flex w-full items-center gap-3 px-3 py-2 text-left transition ${
          selected ? "bg-accent-soft" : "hover:bg-canvas"
        }`}
      >
        {/* The thumbnail is the object store's own URL, the same one the renderer will use. A
            panel that invented a base path here is how a preview works and the site does not. */}
        {/* eslint-disable-next-line @next/next/no-img-element */}
        <img
          src={`/api/v1/media/raw/${candidate.storage_key}`}
          alt=""
          width={56}
          height={40}
          loading="lazy"
          className="h-10 w-14 shrink-0 rounded object-cover"
        />
        <span className="flex min-w-0 flex-1 flex-col leading-tight">
          <span className="truncate text-[12.5px] font-medium">{candidate.filename}</span>
          <span className="truncate text-[11px] text-muted">
            {candidate.width && candidate.height
              ? `${candidate.width}×${candidate.height}`
              : candidate.content_type}
            {candidate.used_by_pages > 0 ? (
              <>
                {" · "}
                {candidate.used_by_pages}{" "}
                {candidate.used_by_pages === 1 ? "page uses" : "pages use"} this
              </>
            ) : null}
          </span>
        </span>
        {selected ? (
          <Check className="size-3.5 shrink-0 text-accent-strong" aria-hidden />
        ) : null}
      </button>
    </li>
  );
}

/** The crop pad: a dot on a grid, draggable, and reachable from the keyboard. */
function FocalPad({
  x,
  y,
  onChange,
  disabled,
}: {
  x: number;
  y: number;
  onChange: (next: { x: number; y: number }) => void;
  disabled: boolean;
}) {
  const pad = useRef<HTMLDivElement | null>(null);
  const [dragging, setDragging] = useState(false);

  /** Turn a pointer position into a 0–1 fraction of the pad. */
  const pointFrom = useCallback((clientX: number, clientY: number) => {
    const box = pad.current?.getBoundingClientRect();
    if (!box || box.width === 0 || box.height === 0) {
      return null;
    }
    return {
      x: Math.min(1, Math.max(0, (clientX - box.left) / box.width)),
      y: Math.min(1, Math.max(0, (clientY - box.top) / box.height)),
    };
  }, []);

  return (
    <div
      ref={pad}
      data-featured-focal-pad
      role="application"
      aria-label="Focal point: the part of the image the crop centres on"
      aria-disabled={disabled}
      tabIndex={disabled ? -1 : 0}
      onPointerDown={(event) => {
        if (disabled) {
          return;
        }
        event.currentTarget.setPointerCapture(event.pointerId);
        setDragging(true);
        const point = pointFrom(event.clientX, event.clientY);
        if (point) {
          onChange(point);
        }
      }}
      onPointerMove={(event) => {
        if (!dragging || disabled) {
          return;
        }
        const point = pointFrom(event.clientX, event.clientY);
        if (point) {
          onChange(point);
        }
      }}
      onPointerUp={(event) => {
        if (event.currentTarget.hasPointerCapture(event.pointerId)) {
          event.currentTarget.releasePointerCapture(event.pointerId);
        }
        setDragging(false);
      }}
      /** The keyboard path. A pad that only answers a pointer is not usable without a mouse,
          and a focal point is exactly the control somebody needs to place precisely. */
      onKeyDown={(event) => {
        if (disabled) {
          return;
        }
        const step = event.shiftKey ? 0.1 : 0.01;
        const moves: Record<string, [number, number]> = {
          ArrowLeft: [-step, 0],
          ArrowRight: [step, 0],
          ArrowUp: [0, -step],
          ArrowDown: [0, step],
        };
        const move = moves[event.key];
        if (!move) {
          return;
        }
        event.preventDefault();
        onChange({
          x: Math.min(1, Math.max(0, x + move[0])),
          y: Math.min(1, Math.max(0, y + move[1])),
        });
      }}
      className={`relative aspect-[3/1] w-full overflow-hidden rounded-lg border border-line bg-canvas ${
        disabled ? "opacity-60" : "cursor-crosshair"
      }`}
    >
      {/* The thirds grid, so a focal point in a third is visible rather than a guess. */}
      <span className="pointer-events-none absolute inset-0 grid grid-cols-3 grid-rows-1">
        <span className="border-r border-line/60" />
        <span className="border-r border-line/60" />
        <span />
      </span>
      <span
        data-featured-focal-dot
        aria-hidden
        className="pointer-events-none absolute size-4 -translate-x-1/2 -translate-y-1/2 rounded-full border-2 border-white bg-accent-strong shadow"
        style={{ left: `${x * 100}%`, top: `${y * 100}%` }}
      />
    </div>
  );
}

/** The media tab of the page editor. */
export function FeaturedMediaTab() {
  const params = useParams<{ id: string }>();
  const pageId = params?.id ?? "";

  const [body, setBody] = useState<FeaturedMediaBody | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [reloadToken, setReloadToken] = useState(0);

  // The picker's rows, fetched only when the operator opens the picker.
  const [candidates, setCandidates] = useState<FeaturedCandidate[] | null>(null);
  const [pickerOpen, setPickerOpen] = useState(false);
  const [pickerError, setPickerError] = useState<string | null>(null);
  const [search, setSearch] = useState("");

  // The form's working copy. Separate from `body` so a half-typed alt is not a save.
  const [mediaId, setMediaId] = useState<string | null>(null);
  const [alt, setAlt] = useState("");
  const [legend, setLegend] = useState("");
  const [focal, setFocal] = useState<{ x: number; y: number } | null>(null);

  const [saving, setSaving] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [confirmRemove, setConfirmRemove] = useState(false);

  const reload = useCallback(() => setReloadToken((token) => token + 1), []);

  useEffect(() => {
    if (!pageId) {
      return;
    }
    let cancelled = false;
    setBody(null);
    setLoadError(null);
    fetchFeaturedMedia(pageId)
      .then((next) => {
        if (cancelled) {
          return;
        }
        setBody(next);
        setMediaId(next.media.media_id);
        setAlt(next.media.alt);
        setLegend(next.media.legend);
        setFocal(
          next.media.focal_x === null || next.media.focal_y === null
            ? null
            : { x: next.media.focal_x, y: next.media.focal_y },
        );
        setNotice(null);
      })
      .catch((cause: unknown) => {
        if (!cancelled) {
          setLoadError(
            cause instanceof ApiError
              ? cause.message
              : "The page's image could not be loaded.",
          );
        }
      });
    return () => {
      cancelled = true;
    };
  }, [pageId, reloadToken]);

  // The picker loads when it is opened, not on mount: a page editor who never touches the image
  // should not pay for the library listing.
  useEffect(() => {
    if (!pickerOpen || !body) {
      return;
    }
    let cancelled = false;
    setCandidates(null);
    setPickerError(null);
    fetchFeaturedCandidates(body.media.site_id, PICKER_PAGE)
      .then((page) => {
        if (!cancelled) {
          setCandidates(page.candidates);
        }
      })
      .catch((cause: unknown) => {
        if (!cancelled) {
          setPickerError(
            cause instanceof ApiError
              ? cause.message
              : "The media library could not be loaded.",
          );
        }
      });
    return () => {
      cancelled = true;
    };
  }, [pickerOpen, body]);

  /** The rows the search leaves standing, matched on the name only. */
  const shownCandidates = useMemo(() => {
    if (!candidates) {
      return [];
    }
    const needle = search.trim().toLowerCase();
    if (!needle) {
      return candidates;
    }
    return candidates.filter((row) => row.filename.toLowerCase().includes(needle));
  }, [candidates, search]);

  const hasImage = mediaId !== null;
  const altBlank = alt.trim().length === 0;
  const overAltCeiling = alt.length > MAX_ALT;
  const overLegendCeiling = legend.length > MAX_LEGEND;
  /** The one reason the save button is off, spelled out rather than a silent disabled control. */
  const blocked =
    hasImage && (altBlank || overAltCeiling || overLegendCeiling) ? "alt" : null;

  /** The preview, from the server's own payload, with the unsaved form overlaid. */
  const preview = useMemo(() => {
    if (!body) {
      return null;
    }
    if (!hasImage) {
      return null;
    }
    const live = body.render;
    if (!live || live.media_id !== mediaId) {
      // The form names a file the server has not answered for yet: the picker is the only way
      // here, so this is the row just clicked and the preview follows on the next save. Showing
      // the previous image while the form names a different one is the alternative, and it is
      // worse than showing nothing.
      return null;
    }
    return { ...live, alt, legend };
  }, [body, hasImage, mediaId, alt, legend]);

  /** The CSS `object-position` the unsaved crop would produce, so the pad and the preview agree. */
  const previewPosition = focal
    ? `${Math.round(focal.x * 100)}% ${Math.round(focal.y * 100)}%`
    : (preview?.object_position ?? null);

  const save = async () => {
    if (!pageId || saving || blocked) {
      return;
    }
    setSaving(true);
    setActionError(null);
    setNotice(null);
    try {
      const changes: Parameters<typeof saveFeaturedMedia>[1] = {
        alt: hasImage ? alt : null,
        legend: hasImage ? legend : null,
      };
      if (mediaId) {
        changes.media_id = mediaId;
      }
      // **The crop is sent only when it moved, and an absent key is what means "leave it".**
      // Sending `null` on every save would clear the crop of every page the editor ever opened.
      if (focal) {
        changes.focal_x = focal.x;
        changes.focal_y = focal.y;
      } else if (body && (body.media.focal_x !== null || body.media.focal_y !== null)) {
        changes.focal_x = null;
        changes.focal_y = null;
      }
      const saved = await saveFeaturedMedia(pageId, changes);
      setBody(saved);
      setMediaId(saved.media.media_id);
      setAlt(saved.media.alt);
      setLegend(saved.media.legend);
      setFocal(
        saved.media.focal_x === null || saved.media.focal_y === null
          ? null
          : { x: saved.media.focal_x, y: saved.media.focal_y },
      );
      setNotice(
        saved.media.media_id
          ? "The image is saved. It is a draft change — publish the page to put it on the site."
          : "The image is removed from the page.",
      );
    } catch (cause: unknown) {
      setActionError(
        cause instanceof ApiError ? cause.message : "The image could not be saved.",
      );
    } finally {
      setSaving(false);
    }
  };

  const remove = async () => {
    if (!pageId || saving) {
      return;
    }
    setSaving(true);
    setActionError(null);
    setNotice(null);
    try {
      const saved = await saveFeaturedMedia(pageId, { clear: true });
      setBody(saved);
      setMediaId(null);
      setAlt("");
      setLegend("");
      setFocal(null);
      setPickerOpen(false);
      setConfirmRemove(false);
      setNotice("The image is removed from the page.");
    } catch (cause: unknown) {
      setActionError(
        cause instanceof ApiError ? cause.message : "The image could not be removed.",
      );
    } finally {
      setSaving(false);
    }
  };

  if (loadError) {
    return (
      <div className="rounded-xl border border-line bg-surface">
        <EmptyState
          title="The page's image could not be loaded"
          hint={loadError}
          action={
            <button
              type="button"
              onClick={reload}
              className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
            >
              Try again
            </button>
          }
        />
      </div>
    );
  }

  if (!body) {
    return (
      <div className="rounded-xl border border-line bg-surface p-6">
        <p className="flex items-center gap-2 text-[12.5px] text-muted">
          <Loader2 className="size-3.5 animate-spin" aria-hidden />
          Loading the page&rsquo;s image…
        </p>
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-4" data-featured-media-tab>
      {actionError ? (
        <p
          role="alert"
          data-featured-error
          className="rounded-xl border border-caution/40 bg-caution-soft px-4 py-3 text-[12.5px]"
        >
          {actionError}
        </p>
      ) : null}
      {notice ? (
        <p
          data-featured-notice
          className="rounded-xl border border-line bg-surface px-4 py-3 text-[12.5px] text-muted"
        >
          {notice}
        </p>
      ) : null}

      {/* The degradation, in the words an operator can act on. */}
      {body.warning ? (
        <p
          role="status"
          data-featured-warning
          className="flex items-start gap-2 rounded-xl border border-caution/40 bg-caution-soft px-4 py-3 text-[12.5px]"
        >
          <AlertTriangle className="mt-0.5 size-3.5 shrink-0" aria-hidden />
          <span>{body.warning}</span>
        </p>
      ) : null}

      <div className="grid gap-4 lg:grid-cols-2">
        {/* ---------------------------------------------------------------------------------
            The left column: the preview and the fields.
        --------------------------------------------------------------------------------- */}
        <div className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-4">
          <div className="flex items-baseline justify-between gap-2">
            <h3 className="text-[13.5px] font-medium">Featured image</h3>
            <span
              data-featured-availability
              className={`rounded-full border px-2 py-0.5 text-[11px] ${chipTone(
                body.availability_label,
              )}`}
            >
              {body.availability_label}
            </span>
          </div>

          {preview ? (
            <figure
              data-featured-preview
              className="overflow-hidden rounded-lg border border-line"
            >
              {/* eslint-disable-next-line @next/next/no-img-element */}
              <img
                src={preview.url}
                alt={preview.alt}
                width={preview.width ?? 1200}
                height={preview.height ?? 400}
                className="aspect-[3/1] w-full object-cover"
                style={previewPosition ? { objectPosition: previewPosition } : undefined}
              />
              {preview.legend ? (
                <figcaption className="border-t border-line px-3 py-2 text-[11.5px] text-muted">
                  {preview.legend}
                </figcaption>
              ) : null}
            </figure>
          ) : (
            <div
              data-featured-empty
              className="flex aspect-[3/1] w-full flex-col items-center justify-center gap-2 rounded-lg border border-dashed border-line bg-canvas text-center"
            >
              <ImageOff className="size-5 text-muted" aria-hidden />
              <p className="text-[12.5px] font-medium">
                {hasImage ? "Saved on the next save" : "No featured image"}
              </p>
              <p className="max-w-[36ch] text-[11.5px] text-muted">
                {hasImage
                  ? "The preview is the renderer's own payload, so it appears once the file is saved."
                  : "A page with no image still renders — it just has no hero above the title."}
              </p>
            </div>
          )}

          <div className="flex flex-wrap items-center gap-2">
            <button
              type="button"
              data-featured-open-picker
              onClick={() => {
                setPickerOpen((open) => !open);
                setPickerError(null);
              }}
              aria-expanded={pickerOpen}
              className="flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
            >
              <Upload className="size-3.5" aria-hidden />
              {hasImage ? "Choose another image" : "Choose an image"}
            </button>
            {hasImage ? (
              <>
                <button
                  type="button"
                  data-featured-clear-crop
                  onClick={() => setFocal(null)}
                  disabled={focal === null}
                  className="flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas disabled:opacity-50 disabled:hover:bg-transparent"
                >
                  <RotateCcw className="size-3.5" aria-hidden />
                  Clear crop
                </button>
                <button
                  type="button"
                  data-featured-remove
                  onClick={() => setConfirmRemove(true)}
                  disabled={saving}
                  className="flex items-center gap-1.5 rounded-lg border border-caution/40 px-3 py-1.5 text-[12.5px] text-caution-strong transition hover:bg-caution-soft disabled:opacity-50"
                >
                  <Trash2 className="size-3.5" aria-hidden />
                  Remove the image
                </button>
              </>
            ) : null}
          </div>

          {confirmRemove ? (
            <div
              data-featured-remove-confirm
              className="flex flex-col gap-2 rounded-lg border border-caution/40 bg-caution-soft px-3 py-3 text-[12.5px]"
            >
              <p>
                This removes the image, its alt text, its legend and its crop from this page. The
                file itself stays in the library.
              </p>
              <div className="flex flex-wrap items-center gap-2">
                <button
                  type="button"
                  data-featured-remove-confirm-yes
                  onClick={() => void remove()}
                  disabled={saving}
                  className="rounded-lg bg-caution-strong px-3 py-1.5 text-[12.5px] font-medium text-white transition disabled:opacity-60"
                >
                  {saving ? "Removing…" : "Yes, remove it"}
                </button>
                <button
                  type="button"
                  onClick={() => setConfirmRemove(false)}
                  className="rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
                >
                  Keep it
                </button>
              </div>
            </div>
          ) : null}
        </div>

        {/* ---------------------------------------------------------------------------------
            The right column: the fields, and the crop.
        --------------------------------------------------------------------------------- */}
        <div className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-4">
          <label className="flex flex-col gap-1">
            <span className="flex items-baseline justify-between gap-2 text-[12px] font-medium">
              <span>
                Alt text{" "}
                {hasImage ? (
                  <span className="font-normal text-muted">(required)</span>
                ) : null}
              </span>
              <span
                className={`text-[11px] ${overAltCeiling ? "text-caution-strong" : "text-muted"}`}
              >
                {alt.length}/{MAX_ALT}
              </span>
            </span>
            <textarea
              id="featured-alt"
              data-featured-alt
              value={alt}
              onChange={(event) => setAlt(event.target.value)}
              rows={2}
              disabled={!hasImage}
              placeholder="What a reader who cannot see the image should be told, in one sentence"
              aria-invalid={hasImage && altBlank}
              className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15 disabled:opacity-60"
            />
            {hasImage && altBlank ? (
              <span className="text-[11px] text-caution-strong">
                A screen reader reads a missing alt as the file name, so an image with no
                description is worse than no image at all.
              </span>
            ) : null}
            {overAltCeiling ? (
              <span className="text-[11px] text-caution-strong">
                {MAX_ALT - alt.length} characters over the limit the API accepts.
              </span>
            ) : null}
          </label>

          {candidates && candidates.find((row) => row.id === mediaId)?.alt_text ? (
            <button
              type="button"
              data-featured-use-file-alt
              onClick={() =>
                setAlt(
                  candidates.find((row) => row.id === mediaId)?.alt_text ?? alt,
                )
              }
              disabled={!hasImage}
              className="self-start rounded-lg border border-line px-2.5 py-1 text-[11.5px] transition hover:bg-canvas disabled:opacity-50"
            >
              Use the file&rsquo;s own alt
            </button>
          ) : null}

          <label className="flex flex-col gap-1">
            <span className="flex items-baseline justify-between gap-2 text-[12px] font-medium">
              <span>Legend</span>
              <span
                className={`text-[11px] ${
                  overLegendCeiling ? "text-caution-strong" : "text-muted"
                }`}
              >
                {legend.length}/{MAX_LEGEND}
              </span>
            </span>
            <input
              id="featured-legend"
              data-featured-legend
              value={legend}
              onChange={(event) => setLegend(event.target.value)}
              disabled={!hasImage}
              placeholder="A caption under the image — optional"
              className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15 disabled:opacity-60"
            />
          </label>

          <div className="flex flex-col gap-2">
            <span className="flex items-center gap-1.5 text-[12px] font-medium">
              <Crosshair className="size-3.5" aria-hidden />
              Focal point
            </span>
            {focal ? (
              <>
                <FocalPad
                  x={focal.x}
                  y={focal.y}
                  disabled={!hasImage}
                  onChange={(next) => setFocal(next)}
                />
                {/* The numbers, editable, for the keyboard path and for a value somebody has
                    been given by a designer. They follow the pad; they do not replace it. */}
                <div className="grid grid-cols-2 gap-2">
                  {(["x", "y"] as const).map((axis) => (
                    <label key={axis} className="flex items-center gap-2 text-[11.5px]">
                      <span className="w-3 text-muted">{axis.toUpperCase()}</span>
                      <input
                        data-featured-focal-x={axis}
                        type="number"
                        min={0}
                        max={100}
                        step={1}
                        value={Math.round((focal[axis] ?? 0) * 100)}
                        disabled={!hasImage}
                        onChange={(event) => {
                          const raw = Number(event.target.value);
                          if (!Number.isFinite(raw)) {
                            return;
                          }
                          const next = Math.min(100, Math.max(0, raw)) / 100;
                          setFocal((current) => ({ x: current?.x ?? 0.5, y: current?.y ?? 0.5, ...{ [axis]: next } }));
                        }}
                        className="w-full rounded-lg border border-line bg-canvas px-2 py-1 text-[12px] outline-none transition focus:border-accent disabled:opacity-60"
                      />
                      <span className="text-muted">%</span>
                    </label>
                  ))}
                </div>
                <p className="text-[11px] text-muted">
                  The crop centres on this point, as a fraction of the image:{" "}
                  <code className="font-mono">
                    object-position: {percent(focal.x)} {percent(focal.y)}
                  </code>
                  . Drag the dot, or use the arrow keys (hold Shift for a tenth at a time).
                </p>
              </>
            ) : (
              <p
                data-featured-no-crop
                className="rounded-lg border border-dashed border-line bg-canvas px-3 py-3 text-[11.5px] text-muted"
              >
                No focal point, so the crop is the image&rsquo;s own centre. Set one when a crop
                would otherwise cut off the subject.
              </p>
            )}
          </div>

          <div className="flex flex-wrap items-center gap-2">
            <button
              type="button"
              data-featured-save
              onClick={() => void save()}
              disabled={saving || blocked !== null}
              className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-quiet-soft disabled:text-muted"
            >
              {saving ? "Saving…" : "Save"}
            </button>
            {blocked ? (
              <span className="text-[11.5px] text-caution-strong">
                The alt text is required before an image can be saved.
              </span>
            ) : (
              <span className="text-[11.5px] text-muted">
                {hasImage
                  ? "Saved to the page's draft. Publish the page to put it on the site."
                  : "Choose an image to add one."}
              </span>
            )}
          </div>
        </div>
      </div>

      {/* The picker. A panel, not a route, because it is a step inside a decision and not a
          destination — and because the fields it fills are on this screen. */}
      {pickerOpen ? (
        <div className="overflow-hidden rounded-xl border border-line bg-surface">
          <div className="flex flex-wrap items-center justify-between gap-3 border-b border-line px-4 py-3">
            <h3 className="text-[13.5px] font-medium">Choose from the library</h3>
            <label className="flex items-center gap-2">
              <span className="sr-only">Search the library by file name</span>
              <input
                data-featured-search
                value={search}
                onChange={(event) => setSearch(event.target.value)}
                placeholder="Search by name"
                className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none transition focus:border-accent"
              />
            </label>
          </div>
          {pickerError ? (
            <div className="flex flex-col items-center gap-3 px-4 py-8 text-center">
              <p className="text-[12.5px] text-caution-strong">{pickerError}</p>
              <button
                type="button"
                onClick={() => {
                  setCandidates(null);
                  setPickerOpen(false);
                  setPickerOpen(true);
                }}
                className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
              >
                Try again
              </button>
            </div>
          ) : candidates === null ? (
            <p className="flex items-center gap-2 px-4 py-8 text-[12.5px] text-muted">
              <Loader2 className="size-3.5 animate-spin" aria-hidden />
              Loading the library…
            </p>
          ) : shownCandidates.length === 0 ? (
            <EmptyState
              title={search ? "No file matches that name" : "This site has no images yet"}
              hint={
                search
                  ? "Clear the search to see every image, or upload a new one from Media."
                  : "Upload a file from the Media screen first — a page can only feature a file the site already holds."
              }
            />
          ) : (
            <ul className="max-h-96 divide-y divide-line overflow-y-auto">
              {shownCandidates.map((candidate) => (
                <CandidateRow
                  key={candidate.id}
                  candidate={candidate}
                  selected={candidate.id === mediaId}
                  onPick={() => {
                    setMediaId(candidate.id);
                    setFocal(null);
                    setPickerOpen(false);
                    setNotice(null);
                    setActionError(null);
                  }}
                />
              ))}
            </ul>
          )}
          {candidates && candidates.length > 0 ? (
            <p className="border-t border-line px-4 py-2 text-[11px] text-muted">
              Showing {shownCandidates.length} of {candidates.length} image
              {candidates.length === 1 ? "" : "s"}
              {candidates.length >= PICKER_PAGE
                ? ` — the newest ${PICKER_PAGE} of the site's library.`
                : "."}
            </p>
          ) : null}
        </div>
      ) : null}

      <p className="text-[11.5px] text-muted">
        {hasImage && body.media.filename ? (
          <>
            <ImageIcon className="mr-1 inline size-3" aria-hidden />
            {body.media.filename}
            {body.media.content_type ? ` · ${body.media.content_type}` : ""}
            {body.media.width && body.media.height
              ? ` · ${body.media.width}×${body.media.height}`
              : ""}
          </>
        ) : (
          "This page features no file, so nothing in the library is reserved for it."
        )}
      </p>
    </div>
  );
}
