import { statusLabel } from "@/lib/format";

/** Tone per lifecycle value the API reports. */
const TONES: Record<string, string> = {
  active: "bg-positive-soft text-positive",
  published: "bg-positive-soft text-positive",
  draft: "bg-caution-soft text-caution",
  suspended: "bg-caution-soft text-caution",
  invited: "bg-caution-soft text-caution",
  // A queued invitation is not a failure and not a live one: it is waiting on a person, which is
  // the same "attention needed" tone as `invited` but says so on the screen. Without a tone it
  // inherited the quiet one and read as "nothing to see here".
  awaiting_approval: "bg-caution-soft text-caution",
  archived: "bg-quiet-soft text-muted",
  disabled: "bg-quiet-soft text-muted",
  stopped: "bg-quiet-soft text-muted",
};

/** A small pill for a lifecycle value (`draft`, `published`, …). */
export function StatusBadge({ status }: { status: string }) {
  const tone = TONES[status] ?? "bg-quiet-soft text-muted";
  return (
    <span
      className={`inline-flex items-center rounded-full px-2 py-0.5 text-[11px] font-medium ${tone}`}
    >
      {statusLabel(status)}
    </span>
  );
}
