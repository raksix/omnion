import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { WebhookListScreen } from "@/features/webhooks/endpoint-list";

export const metadata = { title: "Webhooks" };

export default function WebhooksPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Webhooks"
        description="Where your events are delivered, and whether the receiver is still listening"
      >
        {/* The search and status filters live in the query string, so the screen is read on
            the client and needs a boundary. */}
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the endpoints…</p>}>
          <WebhookListScreen />
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
