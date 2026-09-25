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
  return status.charAt(0).toUpperCase() + status.slice(1);
}
