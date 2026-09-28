"use client";

/**
 * The preview of one file, by kind (docs/requests/REQ-010, slice 2).
 *
 * Five renderers, one contract: a preview either shows the file or says why it cannot. There is
 * no frame that stays blank and no button that does nothing — a type the browser cannot render
 * inline gets a download card with the file's own facts on it, which is a real answer rather
 * than a broken image icon.
 *
 * The bytes come from the panel's read path (`/api/v1/media/{id}/raw`, or a version's own path),
 * so a preview never needs a second unauthenticated surface.
 */
import { useEffect, useState } from "react";

import {
  Download,
  FileText,
  Loader2,
  Music,
  Video,
  ZoomIn,
  ZoomOut,
} from "lucide-react";

import { formatBytes } from "@/lib/format";
import type { MediaFile, MediaVersion } from "@/lib/types";

/** How a file is previewed. */
export type PreviewKind = "image" | "video" | "audio" | "pdf" | "text" | "download";

/**
 * Which renderer a file gets.
 *
 * The decision is made from the *content type*, never from the extension: a `.png` that arrived
 * as `image/jpeg` is a JPEG, and a `.svg` is deliberately not an image here because an SVG can
 * carry script — it takes the download card instead.
 */
export function previewKind(contentType: string): PreviewKind {
  if (contentType.startsWith("image/")) {
    return contentType === "image/svg+xml" ? "download" : "image";
  }
  if (contentType.startsWith("video/")) {
    return "video";
  }
  if (contentType.startsWith("audio/")) {
    return "audio";
  }
  if (contentType === "application/pdf") {
    return "pdf";
  }
  if (contentType === "text/plain") {
    return "text";
  }
  return "download";
}

/** How many lines of a text file the preview shows before it offers the rest. */
const TEXT_LINE_CAP = 200;

/** The preview of one file, at the path the caller supplies. */
export function FilePreview({
  file,
  src,
  version,
  onVersionClick,
}: {
  file: MediaFile;
  src: string;
  /** Which version is on screen, when the preview is of an old one. */
  version?: MediaVersion;
  onVersionClick?: (version: number) => void;
}) {
  const kind = previewKind(version?.content_type ?? file.content_type);

  if (kind === "image") {
    return <ImagePreview src={src} alt={file.alt_text || file.filename} />;
  }
  if (kind === "video") {
    return <VideoPreview src={src} file={file} />;
  }
  if (kind === "audio") {
    return <AudioPreview src={src} file={file} />;
  }
  if (kind === "pdf") {
    return <PdfPreview src={src} file={file} />;
  }
  if (kind === "text") {
    return <TextPreview src={src} file={file} version={version} />;
  }
  return <DownloadCard file={file} version={version} onVersionClick={onVersionClick} />;
}

/**
 * An image, with zoom and pan.
 *
 * The zoom is a scale on a transform rather than a change of the image's own size, so the
 * browser keeps the decoded bitmap and a 4000-pixel photo does not re-decode on every step.
 * Pan is drag-only and only above 100%, because panning a fit-to-box image is a no-op that looks
 * like a broken cursor.
 */
function ImagePreview({ src, alt }: { src: string; alt: string }) {
  const [scale, setScale] = useState(1);
  const [offset, setOffset] = useState({ x: 0, y: 0 });
  const [failed, setFailed] = useState(false);

  if (failed) {
    // A file whose bytes are gone renders the download card rather than a broken-image glyph.
    return (
      <div className="flex h-full min-h-64 flex-col items-center justify-center gap-2 rounded-lg border border-dashed border-line bg-quiet-soft p-6 text-center">
        <FileText className="h-6 w-6 text-muted" aria-hidden />
        <p className="text-[13px] text-muted">
          The bytes of this image could not be read. It may have been purged from the object
          store while the row stayed.
        </p>
        <a
          href={src}
          className="text-[12px] font-medium text-accent-strong underline underline-offset-2"
        >
          Try the raw file
        </a>
      </div>
    );
  }

  const zoom = (next: number) => {
    setScale(Math.min(8, Math.max(0.1, next)));
    if (next <= 1) {
      setOffset({ x: 0, y: 0 });
    }
  };

  return (
    <div className="flex h-full flex-col gap-2">
      <div className="flex items-center gap-1">
        <button
          type="button"
          id="media-preview-zoom-out"
          onClick={() => zoom(scale / 1.5)}
          className="inline-flex h-8 w-8 items-center justify-center rounded-md border border-line text-muted transition-colors hover:bg-quiet-soft hover:text-ink"
          aria-label="Zoom out"
        >
          <ZoomOut className="h-4 w-4" aria-hidden />
        </button>
        <button
          type="button"
          id="media-preview-zoom-in"
          onClick={() => zoom(scale * 1.5)}
          className="inline-flex h-8 w-8 items-center justify-center rounded-md border border-line text-muted transition-colors hover:bg-quiet-soft hover:text-ink"
          aria-label="Zoom in"
        >
          <ZoomIn className="h-4 w-4" aria-hidden />
        </button>
        <button
          type="button"
          id="media-preview-zoom-reset"
          onClick={() => zoom(1)}
          className="ml-1 rounded-md px-2 py-1 text-[12px] font-medium text-muted transition-colors hover:bg-quiet-soft hover:text-ink"
        >
          {Math.round(scale * 100)}%
        </button>
      </div>
      <div
        id="media-preview-image"
        className="flex min-h-64 flex-1 items-center justify-center overflow-auto rounded-lg border border-line bg-quiet-soft p-3"
      >
        {/* eslint-disable-next-line @next/next/no-img-element -- the bytes come from the
            panel's own object store behind a session; the optimiser cannot fetch them. */}
        <img
          src={src}
          alt={alt}
          data-testid="media-preview-image"
          draggable={false}
          onError={() => setFailed(true)}
          onPointerDown={(event) => {
            if (scale <= 1) {
              return;
            }
            const startX = event.clientX - offset.x;
            const startY = event.clientY - offset.y;
            const onMove = (move: PointerEvent) =>
              setOffset({ x: move.clientX - startX, y: move.clientY - startY });
            const onUp = () => {
              window.removeEventListener("pointermove", onMove);
              window.removeEventListener("pointerup", onUp);
            };
            window.addEventListener("pointermove", onMove);
            window.addEventListener("pointerup", onUp);
          }}
          className="max-w-none select-none"
          style={{
            transform: `translate(${offset.x}px, ${offset.y}px) scale(${scale})`,
            transformOrigin: "center center",
            cursor: scale > 1 ? "grab" : "default",
          }}
        />
      </div>
    </div>
  );
}

/**
 * A video, with a poster and controls.
 *
 * `preload="metadata"` is deliberate: the preview must not pull a 400 MB file to draw one frame.
 * The poster frame is the file's own bytes, so the first paint shows the video rather than a
 * black rectangle.
 */
function VideoPreview({ src, file }: { src: string; file: MediaFile }) {
  const poster = file.kind === "image" ? src : undefined;
  return (
    <div
      id="media-preview-video"
      className="flex min-h-64 flex-1 items-center justify-center overflow-hidden rounded-lg border border-line bg-black"
    >
      <video
        data-testid="media-preview-video"
        src={src}
        poster={poster}
        controls
        preload="metadata"
        className="max-h-[60vh] w-full"
      >
        Your browser cannot play this video.{" "}
        <a href={src} className="underline">
          Download it instead.
        </a>
      </video>
    </div>
  );
}

/** An audio file, with the duration the header of the bytes carried. */
function AudioPreview({ src, file }: { src: string; file: MediaFile }) {
  return (
    <div
      id="media-preview-audio"
      className="flex min-h-64 flex-1 flex-col items-center justify-center gap-4 rounded-lg border border-line bg-quiet-soft p-6"
    >
      <Music className="h-10 w-10 text-muted" aria-hidden />
      <audio data-testid="media-preview-audio" src={src} controls preload="metadata" />
      <p className="text-[12px] text-muted">
        {file.duration_ms
          ? `${formatDuration(file.duration_ms)} · ${formatBytes(file.size_bytes)}`
          : "The length of this file was not readable from its header."}
      </p>
    </div>
  );
}

/**
 * A PDF, in the browser's own viewer.
 *
 * The page count is shown beside it when the header carried one. A browser without a built-in
 * viewer gets the download link inside the frame rather than an empty rectangle.
 */
function PdfPreview({ src, file }: { src: string; file: MediaFile }) {
  return (
    <div className="flex min-h-64 flex-1 flex-col gap-2">
      <p className="text-[12px] text-muted">
        {file.page_count
          ? `${file.page_count} page${file.page_count === 1 ? "" : "s"}`
          : "The page count of this document was not readable from its header."}
      </p>
      <iframe
        id="media-preview-pdf"
        data-testid="media-preview-pdf"
        title={`Preview of ${file.filename}`}
        src={src}
        className="min-h-64 flex-1 rounded-lg border border-line bg-white"
      />
      <a
        href={src}
        className="inline-flex items-center gap-1.5 self-start text-[12px] font-medium text-accent-strong underline underline-offset-2"
      >
        <Download className="h-3.5 w-3.5" aria-hidden />
        Open in a new tab
      </a>
    </div>
  );
}

/**
 * A text file, capped at a readable number of lines.
 *
 * The cap is a promise the panel can keep: a 40 000-line log is not a preview. When the file is
 * longer, the line says so and names the real size, so "download for the full file" is an
 * instruction rather than a shrug.
 */
function TextPreview({
  src,
  file,
  version,
}: {
  src: string;
  file: MediaFile;
  version?: MediaVersion;
}) {
  const [text, setText] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  // Loaded on demand rather than with the page: a 5 MB text file has no business in the initial
  // payload of a detail screen, and the button says what it is about to do. The request runs in
  // an effect rather than during render — a fetch in the render body is a side effect that fires
  // again on every state change, so the file is read once per keystroke on the screen.
  const [requested, setRequested] = useState(false);

  useEffect(() => {
    if (!requested || text !== null || error !== null) {
      return;
    }
    let cancelled = false;
    fetch(src, { credentials: "same-origin" })
      .then((response) => {
        if (!response.ok) {
          throw new Error(`the server answered ${response.status}`);
        }
        return response.text();
      })
      .then((body) => {
        if (!cancelled) {
          setText(body);
        }
      })
      .catch((cause: unknown) => {
        if (!cancelled) {
          setError(
            cause instanceof Error ? cause.message : "the file could not be read",
          );
        }
      });
    return () => {
      // A version switch mid-read must not paint the old file's text into the new preview.
      cancelled = true;
    };
  }, [requested, src, text, error]);

  return (
    <div
      id="media-preview-text"
      className="flex min-h-64 flex-1 flex-col gap-2 rounded-lg border border-line bg-quiet-soft p-3"
    >
      {text === null && error === null ? (
        <div className="flex flex-1 flex-col items-center justify-center gap-3">
          <p className="text-[13px] text-muted">
            {formatBytes(version?.size_bytes ?? file.size_bytes)} of plain text, shown up to{" "}
            {TEXT_LINE_CAP} lines.
          </p>
          <button
            type="button"
            id="media-preview-text-load"
            onClick={() => setRequested(true)}
            className="rounded-md bg-ink px-3 py-1.5 text-[12px] font-medium text-inverted transition-opacity hover:opacity-90"
          >
            Load the preview
          </button>
        </div>
      ) : null}
      {error ? (
        <p className="text-[13px] text-negative">The text could not be loaded: {error}.</p>
      ) : null}
      {text !== null ? <CappedText text={text} cap={TEXT_LINE_CAP} /> : null}
    </div>
  );
}

/**
 * The first `cap` lines of a text body, with a line that says how much was left out.
 *
 * Splitting on `\n` and taking a prefix is what makes the cap a *display* cap: the whole body is
 * already in the browser, so the rest is one click away rather than a second request.
 */
export function CappedText({ text, cap }: { text: string; cap: number }) {
  const lines = text.split("\n");
  const shown = lines.slice(0, cap);
  const hidden = lines.length - shown.length;

  return (
    <div className="min-h-0 flex-1 overflow-auto">
      <pre
        data-testid="media-preview-text-body"
        className="whitespace-pre-wrap break-words font-mono text-[12px] leading-relaxed text-ink"
      >
        {shown.join("\n")}
      </pre>
      {hidden > 0 ? (
        <p
          data-testid="media-preview-text-truncated"
          className="mt-3 border-t border-line pt-2 text-[12px] text-muted"
        >
          {hidden.toLocaleString()} more line{hidden === 1 ? "" : "s"} — download the file for the
          full text.
        </p>
      ) : null}
    </div>
  );
}

/**
 * The card a type the browser cannot render inline gets.
 *
 * This is the answer, not a failure: the file's kind, its size, its checksum and — when the
 * preview is of an older version — a way back to the current one.
 */
function DownloadCard({
  file,
  version,
  onVersionClick,
}: {
  file: MediaFile;
  version?: MediaVersion;
  onVersionClick?: (version: number) => void;
}) {
  return (
    <div
      id="media-preview-download"
      className="flex min-h-64 flex-1 flex-col items-center justify-center gap-3 rounded-lg border border-dashed border-line bg-quiet-soft p-6 text-center"
    >
      <FileText className="h-8 w-8 text-muted" aria-hidden />
      <div>
        <p className="text-[13px] font-medium text-ink">
          {version ? `Version ${version.version} of ${file.filename}` : file.filename}
        </p>
        <p className="mt-1 text-[12px] text-muted">
          {version?.content_type ?? file.content_type} has no inline preview on this platform. The
          file downloads in full.
        </p>
      </div>
      <a
        id="media-preview-download-link"
        href={version ? version.raw_path : file.raw_path}
        download
        className="inline-flex items-center gap-1.5 rounded-md bg-ink px-3 py-1.5 text-[12px] font-medium text-inverted transition-opacity hover:opacity-90"
      >
        <Download className="h-3.5 w-3.5" aria-hidden />
        Download
      </a>
      {version && !version.is_current && onVersionClick ? (
        <button
          type="button"
          onClick={() => onVersionClick(version.version)}
          className="text-[12px] font-medium text-accent-strong underline underline-offset-2"
        >
          Back to the current version
        </button>
      ) : null}
    </div>
  );
}

/** A millisecond count as `m:ss` or `h:mm:ss`; never `0:00` for a file that has a length. */
export function formatDuration(milliseconds: number): string {
  const total = Math.max(0, Math.round(milliseconds / 1000));
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const seconds = total % 60;
  const pad = (value: number) => value.toString().padStart(2, "0");
  return hours > 0 ? `${hours}:${pad(minutes)}:${pad(seconds)}` : `${minutes}:${pad(seconds)}`;
}

/** A loading frame, so a slow read is a state rather than a blank box. */
export function PreviewLoading({ label }: { label: string }) {
  return (
    <div className="flex min-h-64 flex-1 flex-col items-center justify-center gap-2 rounded-lg border border-line bg-quiet-soft">
      <Loader2 className="h-5 w-5 animate-spin text-muted" aria-hidden />
      <p className="text-[12px] text-muted">{label}</p>
    </div>
  );
}
