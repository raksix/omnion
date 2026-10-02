import { RequireAuth } from "@/components/require-auth";
import { WorkflowBuilder } from "@/features/workflows/builder-view";

export const metadata = { title: "Workflow builder" };

/**
 * `/workflows/[id]/builder` — the visual builder (REQ-004 slice 1).
 *
 * A full-viewport workspace, so it deliberately does **not** use `AppShell`: the builder's
 * three panes (palette, canvas, inspector) plus its toolbar and problems panel are the whole
 * screen, and a page shell around them would eat the vertical space the canvas needs.
 * `RequireAuth` still wraps it — an unauthenticated visitor is sent to sign in rather than
 * shown an empty canvas that would then fail its first fetch.
 */
export default async function WorkflowBuilderPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = await params;
  return (
    <RequireAuth>
      <WorkflowBuilder workflowId={id} />
    </RequireAuth>
  );
}
