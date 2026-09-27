import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { BlockRegistryView } from "@/features/blocks/block-registry-view";

export const metadata = { title: "Block registry" };

export default function BlocksRoute() {
  return (
    <RequireAuth>
      <AppShell
        title="Block registry"
        description="Every block type the platform ships, with the props each one takes"
      >
        <BlockRegistryView />
      </AppShell>
    </RequireAuth>
  );
}
