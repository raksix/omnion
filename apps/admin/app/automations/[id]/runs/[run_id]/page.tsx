import { RunTrace } from "@/features/automations/operations-views";

export const metadata = { title: "Automation run" };

/**
 * `/automations/[id]/runs/[run_id]` — one run's step trace (docs/requests/REQ-003, slice 4).
 *
 * Two of the request's acceptance criteria can only be proved by *reading* this screen:
 * a step that shows its attempts used against attempts allowed, and a run whose trace says
 * in words why the endless-loop guard stopped it. They are properties of what a human sees,
 * so they get their own route rather than a collapsed panel inside the editor — a trace that
 * is three lines tall in a list row proves nothing.
 */
export default async function AutomationRunPage({
  params,
}: {
  params: Promise<{ id: string; run_id: string }>;
}) {
  const { run_id } = await params;
  return <RunTrace executionId={run_id} />;
}
