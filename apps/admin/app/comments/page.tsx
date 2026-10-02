import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { CommentsView } from "@/features/comments/comments-view";

export const metadata = { title: "Comments" };

/**
 * `/comments` — the moderation queue and the per-site policy (REQ-064, slice 4a).
 *
 * Carries `comments.read` for the queue and `comments.manage` for every write, so the page
 * does not check a permission itself: the route guard answers `403` and the panel renders its
 * own state. A second, client-side permission check would be a second answer to the same
 * question, and the two would eventually disagree.
 */
export default function CommentsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Comments"
        description="What visitors said, and what the filters decided about it"
      >
        <CommentsView />
      </AppShell>
    </RequireAuth>
  );
}
