import { Suspense } from "react";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { AiLocalModelsView } from "@/features/ai/ai-local-models";

export const metadata = { title: "Local AI models" };

/**
 * `/ai/local/models` — what the local endpoints serve (REQ-106, slice 1).
 *
 * Split from `/ai/local` because the two answer different questions at different frequencies: the
 * endpoint list is configuration an operator sets up once, while the model table is what changes
 * when a pull runs. The endpoint filter arrives in the query string, so the models screen has its
 * own `Suspense` boundary — a `useSearchParams` read during a static render is a build error, and
 * the boundary is the fix the other search-param screens in this app already use.
 */
export default function AiLocalModelsPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Local AI models"
        description="What the local endpoints serve, pulled and removed through the endpoint itself — a pull is claimed in the database before the download starts"
      >
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the models…</p>}>
          <AiLocalModelsView />
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
