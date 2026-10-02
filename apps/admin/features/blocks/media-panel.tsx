/**
 * The frame's media panel (REQ-063, slice 4 — "the media-deleted degradation path").
 *
 * The screen answers two questions about the same list of files, and they are different questions
 * with different answers drawn from the same server report:
 *
 * 1. **Which files on this page cannot be served?** A `trashed` file and a `purged` one are both
 *    gone and neither is the author's fault, but only one can be undone. The report distinguishes
 *    them so the panel can say *restore it* for the first and *pick another* for the second — the
 *    same split `featured.rs` draws for a page's single image, and for the same reason: advice that
 *    is wrong is worse than no advice, because it sends the author to a place the file is not.
 *
 * 2. **What happens if I delete this one?** That is the question the degradation exists to answer
 *    and the question nobody could answer before: the only way to find out was to trash a real
 *    file, which changes the page for every visitor and cannot be undone from here. So each row
 *    carries a *Simulate* toggle that names the id in `?media=`, and the server draws the page as
 *    if that file were gone. Nothing is written; the toggle is a viewing mode and it says so.
 *
 * **Why the counts come from the server and not from this list.** The report's `broken_count`
 * counts *files*, while the rows are *references* — a gallery that names one dead file three
 * times is one broken file and three rows. Counting the rows here would print "3 images are gone"
 * for a gallery with one missing picture, which is the number an author then looks for in the
 * media library and cannot find.
 */
"use client";

import { ImageOff, Info, RotateCcw, Trash2 } from "lucide-react";

import type { BlockMediaRef, PagePreview } from "@/lib/types";

/** The two broken states, with the words and the icon each one earns. */
const BROKEN: Record<string, { label: string; className: string }> = {
  trashed: { label: "In the trash", className: "border-caution/40 bg-caution-soft text-caution" },
  purged: { label: "Deleted", className: "border-accent/40 bg-accent-soft text-accent-strong" },
};

/** One row: which file, which block, and what to do about it. */
function Row({
  entry,
  simulated,
  onToggle,
}: {
  entry: BlockMediaRef;
  simulated: boolean;
  onToggle: (id: string) => void;
}) {
  const state = BROKEN[entry.state];
  // A file that is genuinely live but is being *simulated* away has no state badge — the badge
  // would claim a fact about the library that is not true. The row is marked as a simulation
  // instead, which is what it is.
  const simulatedAway = simulated && entry.state === "live";

  return (
    <li
      data-block-media-row
      data-block-media-id={entry.media_id}
      data-block-media-state={entry.state}
      data-block-media-simulated={simulatedAway ? "true" : "false"}
      className="flex flex-wrap items-center gap-x-3 gap-y-1.5 rounded-lg border border-line bg-surface px-3 py-2 text-[12.5px]"
    >
      <span className="min-w-0 flex-1">
        <span className="block truncate font-mono text-[11.5px] text-muted">{entry.path}</span>
        <span className="block text-ink">
          {entry.caption ? `“${entry.caption}”` : entry.media_id.slice(0, 8)}
        </span>
      </span>

      {entry.visible_on !== "none" ? (
        <span
          title={`This block is hidden on ${entry.visible_on === "mobile" ? "phones" : "desktops"}`}
          data-block-media-viewport={entry.visible_on}
          className="inline-flex items-center gap-1 rounded-md bg-canvas px-1.5 py-0.5 text-[11px] text-muted"
        >
          <Info className="size-3" aria-hidden />
          {entry.visible_on === "mobile" ? "phones only" : "desktop only"}
        </span>
      ) : null}

      {state && !simulatedAway ? (
        <span
          data-block-media-badge
          className={`inline-flex items-center gap-1 rounded-md border px-1.5 py-0.5 text-[11px] ${state.className}`}
        >
          {entry.state === "trashed" ? (
            <Trash2 className="size-3" aria-hidden />
          ) : (
            <ImageOff className="size-3" aria-hidden />
          )}
          {state.label}
        </span>
      ) : null}

      {entry.advice && !simulatedAway ? (
        <span data-block-media-advice className="w-full text-[11.5px] text-muted">
          {entry.advice}
        </span>
      ) : null}

      <button
        type="button"
        data-block-media-simulate={entry.media_id}
        onClick={() => onToggle(entry.media_id)}
        aria-pressed={simulatedAway}
        title={
          simulatedAway
            ? "Draw this page as if the file were still there"
            : "Draw this page as if this file had been deleted — nothing is written"
        }
        className={`inline-flex items-center gap-1.5 rounded-lg border px-2.5 py-1 text-[12px] transition ${
          simulatedAway
            ? "border-accent bg-accent-soft text-accent-strong"
            : "border-line text-muted hover:bg-canvas hover:text-ink"
        }`}
      >
        {simulatedAway ? (
          <RotateCcw className="size-3" aria-hidden />
        ) : (
          <ImageOff className="size-3" aria-hidden />
        )}
        {simulatedAway ? "Restore in frame" : "Simulate deletion"}
      </button>
    </li>
  );
}

/**
 * The panel itself.
 *
 * `preview` is the whole frame payload rather than just the media field, because the simulation
 * note needs the server's own counts and the empty state needs to distinguish "this page has no
 * images" from "this page's images are all fine" — two different sentences that both render an
 * empty list.
 */
export function MediaPanel({
  preview,
  simulated,
  onToggle,
}: {
  preview: PagePreview;
  simulated: string[];
  onToggle: (id: string) => void;
}) {
  const refs = preview.media?.refs ?? [];
  // The server's counts, not a tally of the rows: rows are references and files are what an
  // author goes looking for.
  const broken = preview.media_broken_count;
  const live = Math.max(0, preview.media_file_count - broken);

  return (
    <section
      data-block-media-panel
      data-block-media-file-count={preview.media_file_count}
      data-block-media-broken-count={broken}
      data-block-media-simulated-count={preview.simulated_media}
      className="flex flex-col gap-2.5 rounded-xl border border-line bg-panel p-3"
    >
      <header className="flex flex-wrap items-center gap-x-3 gap-y-1">
        <h3 className="text-[13px] font-medium">Images on this page</h3>
        <span data-block-media-counts className="text-[12px] text-muted">
          {live} of {preview.media_file_count} available
          {broken > 0 ? ` · ${preview.media_warning || `${broken} gone`}` : ""}
        </span>
        {preview.simulated_media > 0 ? (
          <span
            data-block-media-simulating
            className="inline-flex items-center gap-1 rounded-md border border-caution/40 bg-caution-soft px-1.5 py-0.5 text-[11px] text-caution"
          >
            Simulating {preview.simulated_media} deleted{" "}
            {preview.simulated_media === 1 ? "file" : "files"}
          </span>
        ) : null}
      </header>

      {broken > 0 ? (
        <p data-block-media-note className="text-[12px] text-muted">
          {preview.media_warning} — the page still renders, and the frame is drawing what a visitor
          would actually get.
        </p>
      ) : null}

      {refs.length === 0 ? (
        <p data-block-media-empty className="text-[12px] text-muted">
          This page has no uploaded images. Blocks pointing at a web address are not checked — the
          platform did not upload those files, so it cannot know when they stop answering.
        </p>
      ) : (
        <ul className="flex flex-col gap-1.5">
          {refs.map((entry) => (
            <Row
              key={`${entry.block_id}:${entry.path}`}
              entry={entry}
              simulated={simulated.includes(entry.media_id)}
              onToggle={onToggle}
            />
          ))}
        </ul>
      )}
    </section>
  );
}
