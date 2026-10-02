import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { RootKeyView } from "@/features/secrets/root-key-view";

export const metadata = { title: "Key ring" };

/**
 * `/secrets/root-key` — the installation key ring and the rotation ceremony (REQ-125, slice 1).
 *
 * The screen is the safe way to do the one irreversible thing the secrets surface allows: a
 * rotation. It runs the seal self-check first, states plainly what losing the operator key
 * costs, and only then opens a resumable re-wrap job with a real counter.
 */
export default function SecretsRootKeyPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Key ring"
        description="The key every stored secret is sealed with — and the rotation that moves it"
      >
        <RootKeyView />
      </AppShell>
    </RequireAuth>
  );
}
