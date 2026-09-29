import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { EndpointDetail } from "@/features/webhooks/endpoint-detail";

export const metadata = { title: "Webhook endpoint" };

export default async function WebhookDetailPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = await params;
  return (
    <RequireAuth>
      <AppShell title="Endpoint" description="Its receiver, its deliveries and what they say about the receiver">
        {/* The tab and the delivery filters live in the query string. */}
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the endpoint…</p>}>
          <EndpointDetail endpointId={id} />
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
