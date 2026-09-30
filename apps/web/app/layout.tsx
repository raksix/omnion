import type { Metadata, Viewport } from "next";
import type { ReactNode } from "react";

import "./globals.css";
// Every bundled theme's stylesheet, loaded together.
//
// A CSS import is a static edge in the module graph and `resolveTheme()` is a runtime call,
// so there is no way to load "only the active theme's" CSS — which is exactly why every sheet
// scopes its element rules to `html[data-theme="<key>"]`. Only the active theme's sheet matches
// anything; the other nine are inert. Without that scoping the last sheet in the bundle would
// win every rule and all ten themes would render with one palette.
import "@omnion/theme-agency/styles.css";
import "@omnion/theme-commerce/styles.css";
import "@omnion/theme-corporate/styles.css";
import "@omnion/theme-documentation/styles.css";
import "@omnion/theme-government/styles.css";
import "@omnion/theme-magazine/styles.css";
import "@omnion/theme-minimal/styles.css";
import "@omnion/theme-portfolio/styles.css";
import "@omnion/theme-startup/styles.css";
import "@omnion/theme-tech/styles.css";
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
 * `data-theme-version`), and that attribute is the hook every theme's stylesheet scopes itself
 * to. The theme's own stylesheet is loaded after the app's reset: presentation belongs to the
 * theme.
 */
export default function RootLayout({ children }: { children: ReactNode }) {
  const theme = resolveTheme();

  return (
    <html lang="en" data-theme={theme.key} data-theme-version={theme.version}>
      <body>{children}</body>
    </html>
  );
}
