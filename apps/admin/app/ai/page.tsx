import { AiView } from "@/features/ai/ai-view";

import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";

export const metadata = { title: "AI Hub" };

export default function AiPage() {
  return (
    <RequireAuth>
      <AppShell
        title="AI Hub"
        description="Providers, the model registry and a chat to try them"
      >
        <AiView />
      </AppShell>
    </RequireAuth>
  );
}
