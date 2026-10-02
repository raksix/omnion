import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { FormInbox } from "@/features/forms/form-inbox";

export const metadata = { title: "Submissions" };

/**
 * `/forms/<id>/submissions` — the inbox (REQ-064, slice 2).
 *
 * A separate route from the builder because it needs a different power: `forms.submissions.read`
 * is not `forms.read`, and a person who may design a contact form has no business reading its
 * replies. Tucking the inbox into the builder as a tab would put a 403 behind a tab label.
 */
export default async function FormSubmissionsPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = await params;
  return (
    <RequireAuth>
      <AppShell title="Submissions" description="What visitors sent back">
        <FormInbox />
      </AppShell>
    </RequireAuth>
  );
}
