import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { SchemaExplorerView } from "@/features/developer/schema-view";

export const metadata = { title: "GraphQL schema" };

/**
 * `/developer/graphql/schema` — the schema explorer (REQ-130, slice 2).
 *
 * The screen the request writes to make the effective-schema rule visible rather than documented:
 * *"Type list grouped by domain, field list showing the permission each field requires, and a
 * `Compare with…` action rendering the field-level diff between two roles."*
 */
export default function GraphqlSchemaPage() {
  return (
    <RequireAuth>
      <AppShell
        title="GraphQL schema"
        description="The types your permissions expose, the ones they withhold, and how that compares with a role"
      >
        <SchemaExplorerView />
      </AppShell>
    </RequireAuth>
  );
}