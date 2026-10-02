import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { DocumentDetailView } from "@/features/developer/documents-view";

export const metadata = { title: "Persisted document" };

/**
 * `/developer/graphql/documents/{id}` — one registered document (REQ-130, slice 2).
 *
 * Next 16 makes the segment a promise, so the page takes the id from its own params rather than
 * reading the URL: a `useParams` here would render on the server with an empty id and then fetch
 * `/graphql/documents/` once the client hydrates.
 */
export default async function GraphqlDocumentPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = await params;
  return (
    <RequireAuth>
      <AppShell
        title="Persisted document"
        description="The text a client runs by id or hash, its operations with their measured cost, and the state that decides whether it executes"
      >
        <DocumentDetailView id={id} />
      </AppShell>
    </RequireAuth>
  );
}