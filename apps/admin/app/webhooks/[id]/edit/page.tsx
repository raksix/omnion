import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { EndpointForm } from "@/features/webhooks/endpoint-form";

export const metadata = { title: "Edit webhook endpoint" };

/**
 * `EndpointForm` reads `?event=` for the catalogue deep link, and `useSearchParams` opts a client
 * component out of the static prerender on **every** route that renders it — not only the one the
 * link points at. The boundary is therefore needed here as well; without it the build fails on
 * this route.
 *
 * The form ignores the parameter when `endpointId` is set (a stored subscription list wins), so
 * the link cannot silently add a subscription to an endpoint that is already live.
 */
export default async function EditWebhookPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = await params;
  return (
    <RequireAuth>
      <AppShell title="Edit the endpoint" description="Its name, receiver, subscriptions and whether it delivers">
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the endpoint…</p>}>
          <EndpointForm endpointId={id} />
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
