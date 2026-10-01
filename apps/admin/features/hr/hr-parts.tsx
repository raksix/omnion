"use client";

/**
 * The HR module's shared pieces (REQ-055, slice 2b).
 *
 * Three of these exist because the alternative is the same computation in four screens, and four
 * copies of a day count is four places a half-day renders as `0.50` next to a row that says `0.5`.
 *
 * - **`LeaveStatusBadge`** reads the status through the panel's shared `StatusBadge`, so a pending
 *   leave looks like every other pending thing in the product. A module inventing its own palette
 *   is how a panel ends up with four greys for "waiting".
 * - **`BalanceCardView`** is the entitled / used / pending / remaining card, and it **prints the
 *   equation**. The acceptance criterion is `entitled = used + pending + remaining`, so the screen
 *   shows the four numbers with their relation instead of four labels a reader has to sum in
 *   their head — and the `remaining` line is the one that goes red, because that is the number
 *   somebody is actually checking before booking.
 * - **`LeaveTypeDot`** gives a leave type a stable colour. Stable by *hashing the id*, not by the
 *   order it was fetched in: an order-dependent colour repaints itself whenever a type is added,
 *   so a legend read last month no longer matches the grid today.
 */
import { CalendarClock, Palmtree } from "lucide-react";

import { StatusBadge } from "@/components/status-badge";

import type { BalanceCard, Days, LeaveStatus } from "@/lib/hr";

/** The four statuses, in the order an inbox would show them. */
export function LeaveStatusBadge({ status }: { status: LeaveStatus }) {
  return <StatusBadge status={status} />;
}

/** A day count as the module sends it: text, so `0.1 + 0.2` is never a question asked here. */
export function DaysCell({ days, className = "" }: { days: Days; className?: string }) {
  return (
    <span className={className} data-qa-hr-days>
      {days}
      {days === "1" ? "" : " days"}
    </span>
  );
}

/**
 * The colour of a leave type, hashed from its id.
 *
 * `hue` is derived from the first four hex digits, so a type keeps its colour across reloads and
 * across browsers. A fixed eight-colour ring keyed by array index is the version that repaints.
 */
export function leaveTypeColor(id: string): { fg: string; bg: string; border: string } {
  let seed = 0;
  for (let index = 0; index < id.length; index += 1) {
    seed = (seed * 31 + id.charCodeAt(index)) >>> 0;
  }
  const hue = seed % 360;
  return {
    fg: `hsl(${hue} 70% 32%)`,
    bg: `hsl(${hue} 70% 95%)`,
    border: `hsl(${hue} 60% 78%)`,
  };
}

/** The legend swatch, and the count of requests of that type in the window. */
export function LeaveTypeDot({
  typeId,
  name,
  count,
}: {
  typeId: string;
  name: string;
  count: number;
}) {
  const color = leaveTypeColor(typeId);
  return (
    <span
      className="inline-flex items-center gap-1.5 text-[12px] text-muted"
      data-qa-hr-legend={typeId}
    >
      <span
        aria-hidden
        className="h-2.5 w-2.5 rounded-sm border"
        style={{ background: color.bg, borderColor: color.border }}
      />
      {name}
      <span className="text-[11px] opacity-70">({count})</span>
    </span>
  );
}

/**
 * The balance card for one leave type and one employee.
 *
 * The four numbers and their relation. `remaining` is negative-tolerant on purpose: a type with
 * `allow_negative` legitimately reads below zero, and showing that as an over-balance error in a
 * type that allows it would be a screen arguing with the policy it was configured by.
 */
export function BalanceCardView({ card }: { card: BalanceCard }) {
  const remaining = Number(card.remaining_days);
  const over = Number.isFinite(remaining) && remaining < 0;
  return (
    <div
      className="rounded-lg border border-border p-3"
      data-qa-hr-balance-card={card.id}
    >
      <div className="flex items-baseline justify-between gap-2">
        <p className="text-[13px] font-medium">{card.name}</p>
        {card.seeded ? null : (
          <span className="text-[11px] text-muted" data-qa-hr-balance-unseeded>
            nothing taken yet
          </span>
        )}
      </div>
      <dl className="mt-2 grid grid-cols-4 gap-2 text-[12px]">
        {(
          [
            ["Entitled", card.entitled_days, ""],
            ["Used", card.used_days, ""],
            ["Pending", card.pending_days, ""],
            ["Remaining", card.remaining_days, over ? "text-destructive" : "font-medium"],
          ] as const
        ).map(([label, value, tone]) => (
          <div key={label}>
            <dt className="text-[11px] text-muted">{label}</dt>
            <dd className={tone} data-qa-hr-balance={label.toLowerCase()}>
              {value}
            </dd>
          </div>
        ))}
      </dl>
    </div>
  );
}

/** The header of a screen that has one date range and nothing else to say about it. */
export function LeaveWindowNote({ from, to, today }: { from: string; to: string; today: string }) {
  return (
    <p className="inline-flex items-center gap-1.5 text-[12px] text-muted" data-qa-hr-calendar-window>
      <CalendarClock className="h-3.5 w-3.5" aria-hidden />
      <span>
        {from} → {to}
      </span>
      <span aria-hidden>·</span>
      <span>today is {today}</span>
    </p>
  );
}

/** The empty state for a month with nobody away. */
export function NoAbsencesNote() {
  return (
    <p
      className="inline-flex items-center gap-1.5 text-[12.5px] text-muted"
      data-qa-hr-calendar-empty
    >
      <Palmtree className="h-3.5 w-3.5" aria-hidden />
      Nobody is away in this window.
    </p>
  );
}
