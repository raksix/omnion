import type { Metadata, Viewport } from "next";
import type { ReactNode } from "react";

import "./globals.css";
import { AppReady } from "@/components/app-ready";
// Both providers, and the order is a claim: the developer gate is about the *account* and the
// environment chip is about the *session's tenant*, so the account question is answered once,
// outside everything that could be scoped to a site.
import { ActiveEnvironmentProvider } from "@/lib/active-environment";
import { DeveloperAccessProvider } from "@/lib/developer-access";
import { SessionProvider } from "@/lib/session";
import { readSessionUser } from "@/lib/session-server";
import { SitesProvider } from "@/lib/sites";
import { TenantStatusProvider } from "@/lib/tenant-status";

export const metadata: Metadata = {
  title: {
    default: "Omnion Admin",
    template: "%s · Omnion Admin",
  },
  description: "Administration panel for an Omnion installation.",
};

export const viewport: Viewport = {
  width: "device-width",
  initialScale: 1,
};

export default async function RootLayout({ children }: { children: ReactNode }) {
  // Resolved on the server, so the browser never probes the session itself.
  const user = await readSessionUser();

  return (
    <html lang="en">
      <body className="min-h-screen bg-canvas font-sans text-[14px] text-ink antialiased">
        <SessionProvider initialUser={user}>
          {/* Inside the session, outside the sites: the question "may this account see the
              Developer group" is about the account, and it is asked once for the whole panel
              rather than by every screen that happens to need it. */}
          <DeveloperAccessProvider>
            <SitesProvider>
              {/* Inside `SitesProvider`, outside nothing: the banner only reads the session, and
                  the switcher it sits beside is the control that changes the answer. */}
              <TenantStatusProvider>
                {/* The chip and the staging banner are panel chrome rather than a screen's own
                    header, so they live beside the freeze notice: both are standing conditions
                    that must be visible on every page, and a provider mounted under a page would
                    be a chip that vanishes on navigation. */}
                <ActiveEnvironmentProvider>{children}</ActiveEnvironmentProvider>
              </TenantStatusProvider>
            </SitesProvider>
          </DeveloperAccessProvider>
        </SessionProvider>
        <AppReady />
      </body>
    </html>
  );
}
