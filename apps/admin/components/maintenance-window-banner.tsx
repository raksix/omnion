"use client";

/**
 * The maintenance-window strip in the app shell (REQ-024, slice 3).
 *
 * The spec asks for "a persistent shell banner" in every admin session, and the reason it says
 * *persistent* is the whole design constraint: a banner that scrolls away is a banner a person
 * reads once and forgets, and this one has to be read every time someone is about to press Save
 * on something that is about to be refused. So it lives inside the sticky header, beside the
 * staging strip, and it has **no dismiss control** — for the same reason the staging strip has
 * none. An operator who is allowed to hide the notice that their writes are being refused will
 * hide it, and then they will be the person who does not understand the 503.
 *
 * It renders nothing when no window is open. That is the important half: a strip that is always
 * present and merely empty is a strip every operator learns to ignore, which costs exactly the
 * attention the feature exists to buy.
 *
 * What it shows, and what it deliberately does not:
 *
 * * The **operator's own message**, never a restatement of it. The same sentence the API returns
 *   in its `503` body is the sentence that belongs here, because the operator wrote it for this
 *   purpose and a paraphrase is a second thing to keep in sync.
 * * The **environment** and the **scope**, because "writes are refused" without either is a
 *   sentence that sends the reader to the deployment centre to find out which environment they
 *   are in. An `admin`-scoped window says so, since that is the difference between "the site is
 *   down for customers" and "I am changing settings".
 * * **Not** a countdown and **not** an end time. A window is open-ended by default, and a
 *   countdown that reaches zero and keeps going is worse than no countdown.
 *
 * The poll is deliberately coarse. A banner that re-reads every 30 seconds is a request per
 * admin session per half minute on an instance that is, by definition, doing nothing — and the
 * window's own API is the one an operator opens to *close* the thing, so it has to be current
 * enough that the banner disappears shortly after the window is closed, and stale enough that
 * thirty open tabs are not thirty requests a minute.
 */

import { useCallback, useEffect, useState } from "react";

import { Wrench } from "lucide-react";
import Link from "next/link";

import { ApiError, fetchDeploymentMaintenance } from "@/lib/api";
import type { DeploymentMaintenanceWindow } from "@/lib/types";

/** How often the strip re-reads the window state. */
const POLL_MS = 30_000;

export function MaintenanceWindowBanner() {
  const [windows, setWindows] = useState<DeploymentMaintenanceWindow[]>([]);

  const load = useCallback(async () => {
    try {
      const response = await fetchDeploymentMaintenance();
      // Only the *active* ones are held in state. Keeping the whole list would mean the strip
      // re-rendered on every poll even when nothing had changed, and — worse — it would make
      // "the window closed" a diff somebody has to notice rather than a list that went empty.
      setWindows(response.active);
    } catch {
      // A strip that cannot read its own state must not say the window is closed. It renders
      // nothing, which is the one honest option available: a wrong "no window" is a lie, and a
      // permanently visible "I could not check" strip is noise. The write routes still enforce
      // the window server-side, so a failed poll costs attention, not safety.
    }
  }, []);

  useEffect(() => {
    void load();
    // The interval is the only thing that would leak here; `load` is stable.
    const timer = window.setInterval(() => void load(), POLL_MS);
    return () => window.clearInterval(timer);
  }, [load]);

  // The strip also re-reads the moment this tab regains focus, because a strict interval can only
  // answer "how stale is this on average" — never "how stale is this now". A poll landing at
  // t=29.9s and one at t=0.1s are the same loop, so the operator who closes a window and looks
  // straight back can be looking at a banner up to a full interval *older* than the one they were
  // promised. That is the exact failure this strip must not have: every write succeeds again and
  // the panel keeps claiming otherwise until somebody believes it. One request for a tab that
  // returns to the foreground, and none for the thirty that do not.
  useEffect(() => {
    const onVisible = () => {
      if (document.visibilityState === "visible") {
        void load();
      }
    };
    document.addEventListener("visibilitychange", onVisible);
    window.addEventListener("focus", onVisible);
    return () => {
      document.removeEventListener("visibilitychange", onVisible);
      window.removeEventListener("focus", onVisible);
    };
  }, [load]);

  if (windows.length === 0) {
    return null;
  }

  return (
    <div
      role="status"
      data-qa-maintenance-banner={windows.map((w) => w.environment).join(",")}
      className="flex flex-wrap items-center gap-x-3 gap-y-1.5 border-b border-caution/30 bg-caution-soft px-4 py-2.5 sm:px-6"
    >
      <Wrench className="size-4 shrink-0 text-caution" aria-hidden />
      <div className="min-w-0 flex-1">
        {windows.map((entry) => (
          <p
            key={entry.environment}
            data-qa-maintenance-banner-row={entry.environment}
            className="text-[12.5px] leading-relaxed text-caution"
          >
            <span className="font-medium capitalize">{entry.environment} — maintenance.</span>{" "}
            {entry.message || "Write routes are refused while this window is open."}{" "}
            {entry.scope === "all"
              ? "Every write is refused; reads and health probes are not affected."
              : "Panel writes are refused; the public site is not affected."}
          </p>
        ))}
      </div>
      <Link
        href="/deployment/maintenance"
        data-qa-maintenance-banner-link
        className="shrink-0 rounded-lg border border-caution/40 px-2.5 py-1 text-[12px] font-medium text-caution transition hover:bg-caution/10"
      >
        Window settings
      </Link>
    </div>
  );
}
