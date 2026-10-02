import { RequireAuth } from "@/components/require-auth";
import { WorkflowTableView } from "@/features/workflows/table-view";

export const metadata = { title: "Workflow table mode" };

/**
 * `/workflows/[id]/table` — the same definition as a list (REQ-004, criterion 8).
 *
 * Like the builder, this is a full-width workspace rather than an `AppShell` page: the table is
 * the whole screen, and the criterion it exists for is that the feature is "fully usable without
 * a pointer" — which a shell's collapsed rail would work against. The route is a **sibling** of
 * the builder rather than a tab inside it, so each view owns one draft and one version, and a
 * save in either place is a save of the same `graph` jsonb.
 */
export default async function WorkflowTablePage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = await params;
  return (
    <RequireAuth>
      <WorkflowTableView workflowId={id} />
    </RequireAuth>
  );
}
