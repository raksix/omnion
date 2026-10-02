import type { Metadata, Viewport } from "next";
import type { ReactNode } from "react";

import "./globals.css";
import "@omnion/theme-minimal/styles.css";
import { resolveTheme } from "@/lib/theme";

export const metadata: Metadata = {
  title: "Omnion",
  description: "A site rendered by Omnion.",
};

export const viewport: Viewport = {
  width: "device-width",
  initialScale: 1,
};

/**
 * The document of the public site.
 *
 * The theme is resolved once per request and announced on `<html>` (`data-theme`,
 * `data-theme-version`), so themes and tooling can hook into the active one. The theme's own
 * stylesheet is loaded after the app's reset: presentation belongs to the theme.
 */
export default function RootLayout({ children }: { children: ReactNode }) {
  const theme = resolveTheme();

  return (
    <html lang="en" data-theme={theme.key} data-theme-version={theme.version}>
      <body>{children}</body>
    </html>
  );
}
