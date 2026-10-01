import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { DeveloperSdksView } from "@/features/developer/developer-sdks-view";

export const metadata = { title: "SDKs and CLI · Developer" };

/**
 * `/developer/sdks` — the four developer tools (REQ-033, slice 4).
 *
 * The tab strip is inside `DeveloperSdksView` rather than in the URL as four separate routes:
 * the four tabs share a generator, a history list and a validator, and four URLs would be four
 * things to remember for four views of the same screen.
 *
 * `Suspense` is required, not decorative. The view reads `?tab=`, because the CLI hands the
 * person `/developer/sdks?tab=cli` to open, and `useSearchParams` opts a client component out of
 * the static prerender — without a boundary this route fails at **build** time, not here. That is
 * the same trap the webhook form's `?event=` deep link walked into earlier this wave.
 */
export default function DeveloperSdksPage() {
  return (
    <RequireAuth>
      <AppShell
        title="SDKs and CLI"
        description="Generate a plugin, theme or workflow starter, validate a manifest, and sign a terminal in"
      >
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the developer tools…</p>}>
          <DeveloperSdksView />
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}