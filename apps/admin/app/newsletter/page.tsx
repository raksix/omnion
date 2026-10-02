import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { NewsletterView } from "@/features/newsletter/newsletter-view";

export const metadata = { title: "Newsletter" };

/**
 * `/newsletter` — lists, subscribers, import/export and the sent archive (REQ-064, slice 4b).
 *
 * Carries `newsletter.read` for the tables and `newsletter.manage` for every write, so the page
 * checks no permission itself: the route guard answers `403` and the panel renders its own
 * state. A second, client-side check would be a second answer to the same question, and the two
 * would eventually disagree.
 */
export default function NewsletterPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Newsletter"
        description="Who is on a list, who is still waiting to confirm, and what has already gone out"
      >
        <NewsletterView />
      </AppShell>
    </RequireAuth>
  );
}
