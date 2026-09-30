"use client";

/**
 * The trend line the health centre draws everywhere it shows a series.
 *
 * This started inside `health-metrics.tsx` and was pulled out for the service
 * detail page, and the reason is worth recording because "just copy the component"
 * is what the second copy of this would have been: two implementations of "what
 * does an unmeasured window look like" is two answers, and the walkthrough asserts
 * the marker attributes (`data-health-spark`) on both screens. A change that
 * taught the metric table to draw a dot for one point would have left the drill-down
 * drawing nothing, and nothing reads as "no data" on a row that has a value.
 *
 * Three rules, each one a way a sparkline lies:
 *
 * 1. **A single point draws a dot.** A `<polyline>` through one point has zero
 *    length and renders as *nothing at all*, which the table then reads as "no
 *    samples" on a row that has one. The walkthrough counts `[data-health-spark]`
 *    elements and asserts the point count matches the series, so this case is
 *    caught rather than assumed.
 * 2. **Scaled between the row's own minimum and maximum, not from zero.** A
 *    sparkline's job is to show *shape*: a queue sitting at 40 and peaking at 60 is
 *    50% busier, and a zero-based chart draws that as a hairline. The scale's
 *    bounds are exposed as `data-*` so a pass can assert a real series was drawn
 *    rather than a flat line pretending to be one.
 * 3. **A window with no samples says so, in words.** An empty box on a row that
 *    has no history is ambiguous — it looks like a chart that failed to load. The
 *    marker says `empty` and the text says why.
 *
 * The `label` prop is the only thing that differs between the two screens, and it
 * exists so the accessible name describes *this* row rather than "sparkline" —
 * twelve rows on a page all named identically is a screen a screen reader cannot be
 * read from.
 */

/** One number, or a dash. Never `0` for a value the platform does not have. */
export function Num({ value }: { value: number | null }) {
  if (value === null) return <span className="text-muted">—</span>;
  return <span className="tabular-nums">{value}</span>;
}

export function Sparkline({
  values,
  label,
  widthClass = "w-24",
}: {
  values: number[];
  /** What this line is the trend of, e.g. `latency`. */
  label: string;
  /** The row's width. The detail page's rows are wider than the metric table's. */
  widthClass?: string;
}) {
  if (values.length === 0) {
    return (
      <span data-health-spark="empty" className="text-[11.5px] text-muted">
        no samples in the last 24 h
      </span>
    );
  }
  const finite = values.filter((value) => Number.isFinite(value));
  if (finite.length === 0) {
    return (
      <span data-health-spark="empty" className="text-[11.5px] text-muted">
        not a number
      </span>
    );
  }

  if (finite.length === 1) {
    return (
      <svg
        data-health-spark="point"
        data-health-spark-points={1}
        data-health-spark-label={label}
        viewBox="0 0 100 30"
        preserveAspectRatio="none"
        role="img"
        aria-label={`${label}: one sample in the last 24 hours`}
        className={`h-8 ${widthClass} text-muted`}
      >
        <circle cx="50" cy="15" r="2" fill="currentColor" />
      </svg>
    );
  }

  const low = Math.min(...finite);
  const high = Math.max(...finite);
  const span = high - low;
  const step = 100 / (finite.length - 1);
  const points = finite
    .map((value, index) => {
      const y = span === 0 ? 15 : 28 - ((value - low) / span) * 26;
      return `${(index * step).toFixed(2)},${y.toFixed(2)}`;
    })
    .join(" ");

  return (
    <svg
      data-health-spark="line"
      data-health-spark-points={finite.length}
      data-health-spark-label={label}
      data-health-spark-min={low}
      data-health-spark-max={high}
      viewBox="0 0 100 30"
      preserveAspectRatio="none"
      role="img"
      aria-label={`${label}: ${finite.length} samples in the last 24 hours, from ${low} to ${high}`}
      className={`h-8 ${widthClass} text-ink`}
    >
      <polyline
        points={points}
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
        vectorEffect="non-scaling-stroke"
      />
    </svg>
  );
}
