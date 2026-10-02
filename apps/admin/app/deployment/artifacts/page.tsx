import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { ArtifactsView } from "@/features/deployment/artifacts-view";

export const metadata = { title: "Release artifacts" };

/**
 * `/deployment/artifacts` — the cached releases and the artifacts each one published
 * (REQ-128, slice 4).
 *
 * The screen exists for one comparison: is the digest on this screen the digest in the registry.
 * That is why every row carries a copy button that hands over the whole reference, why a kind a
 * release did not publish renders as an explicit row rather than a gap, and why each release shows
 * when its manifest was last read — an operator comparing against a registry needs to know
 * whether they are reading this minute or an hour ago.
 */
export default function DeploymentArtifactsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Release artifacts"
        description="The images, CLI binaries, chart and SBOMs each cached release published, with the digest to pin each one"
      >
        <ArtifactsView />
      </AppShell>
    </RequireAuth>
  );
}
