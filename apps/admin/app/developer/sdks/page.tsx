import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { DeveloperSdksView } from "@/features/developer/developer-sdks-view";

export const metadata = { title: "SDKs and CLI · Developer" };

/**
 * `/developer/sdks` — the four developer tools (REQ-033, slice 4).
 *
 * The tab strip is inside `DeveloperSdksView` rather than in the URL: the four tabs share a
 * generator, a history list and a validator, and four routes would mean four URLs to remember for
 * four views of the same screen. The tab that a developer arrives at most often — the CLI — is
 * the last one, so the default is a starter, which is the more common question.
 *
 * No `Suspense` boundary: nothing here reads the query string on the client, so there is nothing
 * to suspend on and a boundary would only add a frame of loading for a screen that hydrates
 * immediately.
 */
export default function DeveloperSdksPage() {
  return (
    <RequireAuth>
      <AppShell
        title="SDKs and CLI"
        description="Generate a plugin, theme or workflow starter, validate a manifest, and sign a terminal in"
      >
        <DeveloperSdksView />
      </AppShell>
    </RequireAuth>
  );
}
