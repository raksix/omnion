import type { Metadata, Viewport } from "next";
import type { ReactNode } from "react";

import "./globals.css";
import { AppReady } from "@/components/app-ready";
import { DeveloperAccessProvider } from "@/lib/developer-access";
import { SessionProvider } from "@/lib/session";
import { readSessionUser } from "@/lib/session-server";
import { SitesProvider } from "@/lib/sites";

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
            <SitesProvider>{children}</SitesProvider>
          </DeveloperAccessProvider>
        </SessionProvider>
        <AppReady />
      </body>
    </html>
  );
}
