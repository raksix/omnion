import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AuditView } from "@/features/secrets/audit-view";

export const metadata = { title: "Secrets audit" };

/**
 * `/secrets/audit` — the access trail, the advisory flags and the SIEM export
 * (REQ-125, slice 4).
 *
 * The screen answers "who touched which secret, from where, under which request id" and shows the
 * four reveal detectors' flags with an acknowledge action. The only write on it is clearing a flag,
 * nothing here can return a credential value, and the export is built from an explicit field list
 * so a column added to the trail later cannot leak into it.
 */
export default function SecretsAuditPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Secrets audit"
        description="Every access, every refusal, and the flags the reveal detectors raised — metadata only"
      >
        <AuditView />
      </AppShell>
    </RequireAuth>
  );
}
