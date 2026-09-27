"use client";

/**
 * Pieces the media screens share: the type icon, the scan badge tone and the trash countdown.
 *
 * They live here rather than in one screen so the browser and the trash read the same way — a
 * `flagged` file has to look flagged wherever it appears.
 */
import { FileText, Image as ImageIcon, Music, Video } from "lucide-react";

import type { MediaFile } from "@/lib/types";

/** The icon a file's content type shows. */
export function fileIcon(file: MediaFile) {
  switch (file.kind) {
    case "image":
      return ImageIcon;
    case "video":
      return Video;
    case "audio":
      return Music;
    default:
      return FileText;
  }
}

/** The tone a scan badge is drawn in, so a flagged file is never just another row. */
export function scanTone(status: string): string {
  switch (status) {
    case "clean":
      return "bg-positive-soft text-positive";
    case "flagged":
      return "bg-caution-soft text-caution";
    case "error":
      return "bg-accent-soft text-accent-strong";
    default:
      return "bg-quiet-soft text-muted";
  }
}

/** Whole days between now and a timestamp, never below zero; `null` when there is no date. */
export function daysUntil(value: string | null): number | null {
  if (!value) {
    return null;
  }
  const parsed = new Date(value).getTime();
  if (Number.isNaN(parsed)) {
    return null;
  }
  return Math.max(0, Math.ceil((parsed - Date.now()) / 86_400_000));
}

/** The scan badge every media screen shows on a row. */
export function ScanBadge({ status }: { status: string }) {
  return (
    <span
      className={`inline-flex items-center rounded-full px-2 py-0.5 text-[11px] font-medium ${scanTone(status)}`}
    >
      {status}
    </span>
  );
}
