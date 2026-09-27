import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { DevicesView } from "@/features/iam/devices-view";

export const metadata = { title: "Devices" };

/**
 * The device registry (REQ-006, slice 3): what the platform has seen, its trust window, and how
 * to forget a device it should not trust again.
 */
export default function IamDevicesPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Devices"
        description="Known devices, their trust windows and the sessions they hold"
      >
        <DevicesView />
      </AppShell>
    </RequireAuth>
  );
}
