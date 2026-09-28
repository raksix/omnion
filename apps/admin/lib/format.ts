/** Small presentation helpers shared by the admin views. */

const timestampFormatter = new Intl.DateTimeFormat("en", {
  dateStyle: "medium",
  timeStyle: "short",
});

/** Render an RFC 3339 timestamp the API answered with; the raw value survives bad input. */
export function formatTimestamp(value: string): string {
  const parsed = new Date(value);
  if (Number.isNaN(parsed.getTime())) {
    return value;
  }
  return timestampFormatter.format(parsed);
}

/** Turn a status key into the label the panel shows. */
export function statusLabel(status: string): string {
  if (!status) {
    return "unknown";
  }
  // Underscores become spaces and every word is capitalised, because the API's vocabulary is
  // `snake_case` and this is the one place that vocabulary reaches a screen. A status the panel
  // has never seen (`awaiting_approval`) used to render as `Awaiting_approval`, which reads as a
  // leaked token rather than a state — and the value was correct, so nothing else would have
  // caught it. Splitting on `_` and `-` covers both, since scope and role keys use dashes too.
  return status
    .split(/[_-]/)
    .filter((word) => word.length > 0)
    .map((word) => word.charAt(0).toUpperCase() + word.slice(1))
    .join(" ");
}

/** Render a byte count the way the panel shows file sizes. */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) {
    return "—";
  }
  if (bytes < 1024) {
    return `${bytes} B`;
  }

  const units = ["KB", "MB", "GB", "TB"] as const;
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value >= 10 ? Math.round(value) : value.toFixed(1)} ${units[unit]}`;
}
