import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { DeprecationsView } from "@/features/developer/deprecations-view";

export const metadata = { title: "API deprecations" };

/**
 * `/developer/api/deprecations` — the versioned API policy (REQ-130, slice 4).
 *
 * The screen exists because a deprecation that only lives in a database row is invisible to the
 * two parties it concerns: the integrator reading their client errors, and the operator who
 * announced it and forgot. Everything on this page is read from the same rows the middleware
 * matches on the request path, so a sunset shown here and a sunset enforced there cannot differ.
 */
export default function DeprecationsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="API deprecations"
        description="Announced and removed routes, their sunsets and their replacements — the same rows the response headers are built from"
      >
        <DeprecationsView />
      </AppShell>
    </RequireAuth>
  );
}
