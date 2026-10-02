import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { DocumentsView } from "@/features/developer/documents-view";

export const metadata = { title: "Persisted documents" };

/**
 * `/developer/graphql/documents` — the persisted-document manager (REQ-130, slice 2).
 *
 * A persisted document is one a client runs by id or hash instead of sending text. With
 * persisted-only mode on it is the ONLY thing that executes, so this list is the allowlist an
 * operator reads to answer "what can a client run against this installation".
 */
export default function GraphqlDocumentsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Persisted documents"
        description="The allowlist a client runs by id or hash, with the hash it sends and the state that decides whether it executes"
      >
        <DocumentsView />
      </AppShell>
    </RequireAuth>
  );
}