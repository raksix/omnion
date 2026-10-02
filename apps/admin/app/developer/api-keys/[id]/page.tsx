import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { DeveloperKeyDetailScreen } from "@/features/developer/developer-key-detail";

export const metadata = { title: "API key" };

/**
 * One key.
 *
 * The id is a route parameter, so this page is a dynamic segment. Next renders it per request,
 * which is what lets the screen ask the API for exactly this key — and it is why the screen is a
 * client component: the panel's session is an HttpOnly cookie the server component cannot pass
 * down, and every other screen in this app reads its data on the client for the same reason.
 */
export default async function DeveloperKeyPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = await params;

  return (
    <RequireAuth>
      <AppShell
        title="API key"
        description="What this key can do, how much it has used, and the requests it has made"
      >
        <DeveloperKeyDetailScreen id={id} />
      </AppShell>
    </RequireAuth>
  );
}
