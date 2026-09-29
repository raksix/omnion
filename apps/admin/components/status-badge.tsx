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
  // CDN purge states (REQ-011). Without tones here every one of the five rendered in the
  // quiet grey, which makes a `failed` purge and a `succeeded` one look identical in a
  // history table — the exact reading the screen exists to prevent. `succeeded` is the
  // only positive; `queued` and `running` are in flight and get the attention tone so a
  // queue that is not draining is visible without opening a row.
  succeeded: "bg-positive-soft text-positive",
  done: "bg-positive-soft text-positive",
  queued: "bg-caution-soft text-caution",
  running: "bg-caution-soft text-caution",
  partial: "bg-caution-soft text-caution",
  pending: "bg-quiet-soft text-muted",
  failed: "bg-accent-soft text-accent-strong",
  // Environments (REQ-017). `cloning` is work in flight, so it wears the attention tone rather
  // than the quiet one: without it a copying environment read exactly like a live one, and the
  // only way to tell them apart was to open the row. `error` is a failure even though the API
  // spells it as a state rather than a past tense, so it must not fall through to grey.
  cloning: "bg-caution-soft text-caution",
  error: "bg-accent-soft text-accent-strong",
  cancelled: "bg-quiet-soft text-muted",
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
