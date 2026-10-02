"use client";

/**
 * The suspended/archived banner (REQ-005, slice 3).
 *
 * It is rendered above the header's content on every signed-in screen, not on the tenant's
 * settings page, because the acceptance line is about the whole panel: a frozen tenant that
 * only says so on the page where the freeze was set is a frozen tenant that looks broken on
 * every other page. It is `role="status"` rather than `role="alert"` on purpose — it is a
 * standing condition, not an event that just happened, and an alert that re-announces itself on
 * every navigation is noise a screen reader user pays for on every screen.
 */
import { PauseCircle } from "lucide-react";

import { useTenantStatus } from "@/lib/tenant-status";

/** The strip that says a tenant accepts no writes. Renders nothing when it is active. */
export function TenantStatusBanner() {
  const { isFrozen, reason, status } = useTenantStatus();

  if (!isFrozen) {
    return null;
  }

  return (
    <div
      role="status"
      data-qa-tenant-banner={status ?? ""}
      className="flex items-start gap-2.5 border-b border-caution/30 bg-caution-soft px-4 py-2.5 sm:px-6"
    >
      <PauseCircle className="mt-px size-4 shrink-0 text-caution" aria-hidden />
      <p className="text-[12.5px] leading-relaxed text-caution">
        <span className="font-medium capitalize">{status}</span> — {reason}
      </p>
    </div>
  );
}
