import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { EndpointForm } from "@/features/webhooks/endpoint-form";

export const metadata = { title: "New webhook endpoint" };

/**
 * The form reads `?event=` to preselect a subscription when a developer follows the catalogue's
 * deep link, and `useSearchParams` opts a client component out of the static prerender — without
 * a boundary the build fails on this route at deploy time rather than here. The fallback is the
 * same shape the event feed uses, so the page is never a blank frame.
 */
export default function NewWebhookPage() {
  return (
    <RequireAuth>
      <AppShell title="Connect an endpoint" description="A URL that receives one signed POST per subscribed event">
        {/* The form keeps the state the API answers with (including a secret it will never
            return again), so it must live inside the client tree the AppShell renders. */}
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the form…</p>}>
          <EndpointForm />
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
