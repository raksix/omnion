import { Suspense } from "react";

import { AiView } from "@/features/ai/ai-view";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";

export const metadata = { title: "AI Hub" };

/**
 * The AI Hub. Its chat reads `?q=…` (the palette's `Ask AI` row hands the words over), so the
 * view sits inside a Suspense boundary — the same reason the results screen does.
 */
export default function AiPage() {
  return (
    <RequireAuth>
      <AppShell
        title="AI Hub"
        description="Providers, the model registry and a chat to try them"
      >
        <Suspense fallback={<p className="text-[13px] text-muted">Loading the AI Hub…</p>}>
          <AiView />
        </Suspense>
      </AppShell>
    </RequireAuth>
  );
}
