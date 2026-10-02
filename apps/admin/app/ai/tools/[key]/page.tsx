import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AiToolDetailView } from "@/features/ai/ai-tool-detail";

export const metadata = { title: "Tool" };

/**
 * One tool's detail screen (REQ-100, slice 1).
 *
 * Four sections: the argument schema verbatim, the agents whose allow-list names this tool, the
 * recent call log with arguments redacted, and the four limits an operator owns. The page reads
 * the key from the route and hands it to the view as a prop — a client component that read
 * `useParams` would make the key unavailable during the first render, which is exactly when the
 * skeleton has to show.
 */
export default async function AiToolDetailPage({
  params,
}: {
  params: Promise<{ key: string }>;
}) {
  const { key } = await params;
  return (
    <RequireAuth>
      <AppShell
        title={key}
        description="The arguments this tool takes, the agents that may call it, what it has done and what it is allowed to do"
      >
        <AiToolDetailView toolKey={decodeURIComponent(key)} />
      </AppShell>
    </RequireAuth>
  );
}
