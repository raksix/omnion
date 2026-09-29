import { AppShell } from "@/components/app-shell";
import { RequireAuth } from "@/components/require-auth";
import { StorefrontSettingsScreen } from "@/features/ecommerce/storefront-settings";

export const metadata = { title: "Storefront" };

export default function CommerceStorefrontPage() {
  return (
    <RequireAuth>
      <AppShell
        title="Storefront"
        description="The per-site shop configuration the public site serves on every request"
      >
        <StorefrontSettingsScreen />
      </AppShell>
    </RequireAuth>
  );
}
