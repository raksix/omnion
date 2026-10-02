import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { EndpointForm } from "@/features/webhooks/endpoint-form";

export const metadata = { title: "Edit webhook endpoint" };

export default async function EditWebhookPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = await params;
  return (
    <RequireAuth>
      <AppShell title="Edit the endpoint" description="Its name, receiver, subscriptions and whether it delivers">
        <EndpointForm endpointId={id} />
      </AppShell>
    </RequireAuth>
  );
}
