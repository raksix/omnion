import { AutomationsView } from "@/features/automations/automations-view";

export const metadata = { title: "Automation" };

/**
 * `/automations/[id]` — one rule, opened from the list.
 *
 * The list links here, so this route is not optional: a row that points at a 404 is a dead
 * button. The rule is opened in the same editor the list uses, and a rule that does not exist
 * (or belongs to another organization) is reported as such rather than as an empty editor.
 *
 * The run history, the run detail and the audit tab are slice 2's and slice 4's; they are
 * reachable from this page's own links once those land, and the tab strip is not drawn until
 * there is something behind every tab.
 */
export default async function AutomationDetailPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = await params;
  return <AutomationsView openId={id} />;
}
