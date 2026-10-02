import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { RevisionHistoryView } from "@/features/blocks/revision-history-view";

export const metadata = { title: "Revisions" };

/** The revision history of one page, with the block-level compare between two of them. */
export default async function PageRevisionsRoute({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = await params;
  return (
    <RequireAuth>
      <AppShell
        title="Revisions"
        description="Every save appends a revision; comparing two of them shows what actually changed"
      >
        <RevisionHistoryView pageId={id} />
      </AppShell>
    </RequireAuth>
  );
}
