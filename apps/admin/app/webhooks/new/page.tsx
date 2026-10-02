import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { EndpointForm } from "@/features/webhooks/endpoint-form";

export const metadata = { title: "New webhook endpoint" };

export default function NewWebhookPage() {
  return (
    <RequireAuth>
      <AppShell title="Connect an endpoint" description="A URL that receives one signed POST per subscribed event">
        {/* The form keeps the state the API answers with (including a secret it will never
            return again), so it must live inside the client tree the AppShell renders. */}
        <EndpointForm />
      </AppShell>
    </RequireAuth>
  );
}
