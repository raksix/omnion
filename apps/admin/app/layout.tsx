import type { Metadata, Viewport } from "next";
import type { ReactNode } from "react";

import "./globals.css";
import { AppReady } from "@/components/app-ready";
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
          <SitesProvider>
            {/* Inside `SitesProvider`, outside nothing: the banner only reads the session, and
                the switcher it sits beside is the control that changes the answer. */}
            <TenantStatusProvider>{children}</TenantStatusProvider>
          </SitesProvider>
        </SessionProvider>
        <AppReady />
      </body>
    </html>
  );
}
