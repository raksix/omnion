import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { BackfillDetailView } from "@/features/deployment/backfill-detail-view";

export const metadata = { title: "Backfill detail" };

/**
 * `/deployment/backfills/{id}` — one job and the statement it runs (REQ-129, slice 3).
 *
 * `params` is awaited because the admin app is Next 16: in a server component the route params are
 * a promise, and reading them without awaiting them yields a Promise object that renders as
 * `[object Promise]` in the detail fetch.
 */
export default async function BackfillDetailPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = await params;

  return (
    <RequireAuth>
      <AppShell
        title="Backfill detail"
        description="The cursor a resume continues from, the rows written so far, and the statement the next batch runs"
      >
        <BackfillDetailView id={id} />
      </AppShell>
    </RequireAuth>
  );
}
