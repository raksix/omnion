import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { InstallView } from "@/features/deployment/install-view";

export const metadata = { title: "Install bundle" };

/**
 * `/deployment/install` — the environment bundle generator (REQ-128, slice 4).
 *
 * The request's own rule is the screen's headline: generated files contain **references** to
 * secrets, never values, and the screen says so in three places rather than one — on the form,
 * on the result and beside the download. "Does my generated file contain my database password" is
 * the question every operator asks exactly once and nobody wants to answer by reading YAML.
 *
 * The size preset shows its concrete numbers because the request asks for the numbers rather than
 * the name: an operator sizing a node from a preset called "medium" is guessing.
 */
export default function DeploymentInstallPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Install bundle"
        description="Generate a target's compose stack or Helm values, its checksums and the commands to run it — with secret references, never values"
      >
        <InstallView />
      </AppShell>
    </RequireAuth>
  );
}
