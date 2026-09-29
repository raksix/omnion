import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { FormBuilder } from "@/features/forms/form-builder";

export const metadata = { title: "Build form" };

/**
 * `/forms/<id>/edit` — the form builder (REQ-064, slice 2).
 *
 * The route carries the form's id, which is why it is not in the walkthrough's route list: a
 * route walked with a placeholder id would only prove that the 404 state renders. The builder is
 * driven by a depth pass that creates a real form first, exactly as the menu editor is.
 */
export default async function FormEditPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = await params;
  return (
    <RequireAuth>
      <AppShell title="Build form" description="Fields, validation and what happens after a send">
        <FormBuilder formId={id} />
      </AppShell>
    </RequireAuth>
  );
}
