import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { FormsView } from "@/features/forms/forms-view";

export const metadata = { title: "Forms" };

/**
 * `/forms` — the site's forms and their submission counts (REQ-064, slice 2).
 *
 * Carries `forms.read` for the list and `forms.manage` for every write, so the page does not
 * check a permission itself: the route guard answers 403 and the panel renders its own state. A
 * second, client-side permission check would be a second answer to the same question, and the two
 * would eventually disagree — and `forms.submissions.read` is a *third* power, which is why the
 * inbox is its own route rather than a tab that quietly fails.
 */
export default function FormsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Forms"
        description="What a visitor can send back, and what has arrived"
      >
        <FormsView />
      </AppShell>
    </RequireAuth>
  );
}
