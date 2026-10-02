import { AutomationTemplatesView } from "@/features/automations/operations-views";

export const metadata = { title: "Automation templates" };

/**
 * `/automations/templates` — the six starter rules (docs/requests/REQ-003, slice 4).
 *
 * A separate route rather than a tab on the editor, because a template is not a state a
 * rule is in: it is a definition you *install*, and installing one is an ordinary create
 * through the same endpoint a hand-written rule takes. A tab would suggest a template is
 * something a rule becomes.
 */
export default function AutomationTemplatesPage() {
  return <AutomationTemplatesView />;
}
