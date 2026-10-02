import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { WorkflowCanvas } from "@/features/workflows/workflow-canvas";

export const metadata = { title: "Workflow editor" };

export default async function WorkflowEditorPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  // The id is read on the server only to name the shell; the canvas fetches the graph itself
  // from the same endpoint every other screen uses, so there is one source of the document and
  // a revision read on the server cannot drift from the one the canvas saves against.
  const { id } = await params;
  return (
    <RequireAuth>
      <AppShell title="Workflow editor" description="Build the graph, then run it">
        <WorkflowCanvas workflowId={id} />
      </AppShell>
    </RequireAuth>
  );
}
