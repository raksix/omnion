import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { NodeDetailPage } from "@/features/nodes/node-detail";

export const metadata = { title: "Node" };

export default async function NodePage({
  params,
}: {
  params: Promise<{ key: string }>;
}) {
  // The key is read on the server only to name the shell; the definition itself is fetched by
  // the client component from the same endpoint the palette uses, so there is one source.
  const { key } = await params;
  return (
    <RequireAuth>
      <AppShell
        title="Node"
        description="What one node takes, what it returns, and what it needs from you"
      >
        <NodeDetailPage params={params} />
      </AppShell>
    </RequireAuth>
  );
}
