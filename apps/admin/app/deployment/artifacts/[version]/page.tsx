import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { ReleaseDetailView } from "@/features/deployment/artifacts-view";

export const metadata = { title: "Release" };

/**
 * `/deployment/artifacts/{version}` — one release in full (REQ-128, slice 4).
 *
 * The screen carries the four facts a release is a contract for: the commit every artifact was
 * built from, the lowest core version it runs on, the migrations it ships and where its notes live.
 * The core minimum is compared against **this build** here rather than in the list, because that
 * comparison is the whole reason to look at a release before planning an upgrade to it.
 *
 * Next 16 makes the segment a promise, so the page takes the version from its own params rather
 * than reading the URL: a `useParams` here would render on the server with an empty version and
 * then fetch `/deployment/artifacts/` once the client hydrates.
 */
export default async function DeploymentReleasePage({
  params,
}: {
  params: Promise<{ version: string }>;
}) {
  const { version } = await params;
  return (
    <RequireAuth>
      <AppShell title={`Release ${version}`} description="What one release published, and what it needs from this instance">
        <ReleaseDetailView version={version} />
      </AppShell>
    </RequireAuth>
  );
}
