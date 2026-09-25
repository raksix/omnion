import type { Metadata, Viewport } from "next";
import type { ReactNode } from "react";

import "./globals.css";
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
          <SitesProvider>{children}</SitesProvider>
        </SessionProvider>
      </body>
    </html>
  );
}
