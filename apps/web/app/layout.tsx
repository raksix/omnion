import type { Metadata, Viewport } from "next";
import { headers } from "next/headers";
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
import { getSiteTheme } from "@/lib/api";
import { resolveThemeOrWarn } from "@/lib/theme";
import { SITE_HINT_HEADER } from "@/proxy";

export const metadata: Metadata = {
  title: "Omnion",
  description: "A site rendered by Omnion.",
};

export const viewport: Viewport = {
  width: "device-width",
  initialScale: 1,
};

type LayoutProps = { children: ReactNode; params?: Promise<unknown> };

/**
 * The document of the public site.
 *
 * The theme is resolved once per request and announced on `<html>` (`data-theme`,
 * `data-theme-version`), and that attribute is the hook every theme's stylesheet scopes itself
 * to. The theme's own stylesheet is loaded after the app's reset: presentation belongs to the
 * theme.
 *
 * ## The attribute IS the theme system
 *
 * All ten stylesheets ship in one bundle and every element rule in them is scoped to
 * `html[data-theme="<key>"]`, so this one attribute decides which sheet paints anything at all.
 * It therefore has to be written from the *addressed site's* theme — the same value
 * `app/[[...slug]]/page.tsx` draws the page with.
 *
 * It used to be written from `resolveTheme()` with no argument, which is the installation-wide
 * default, and that made every non-default site render as a colour-swapped copy of Minimal: the
 * page component rendered Magazine's markup, but no `html[data-theme="magazine"]` block ever
 * matched, so every theme token came back unset and the body inherited Minimal's palette and
 * sans-serif stack. REQ-062 acceptance 1 asks for the opposite — "that theme's layout, not a
 * colour-swapped copy" — and the swap was total, not cosmetic. Measured with all ten real
 * sheets: `--ma-magazine-canvas` and `--ma-magazine-font-title` go from `#fffdf9` / a serif stack
 * to unset, and `body` goes from `rgb(255, 253, 249)` / serif to `rgb(250, 249, 245)` /
 * `ui-sans-serif`. Nothing in the response said so: the page was complete, the request
 * succeeded, and the only clue was an attribute nobody compared against the chosen theme.
 *
 * ## Why the site hint comes from a header
 *
 * A layout cannot read `searchParams` (Next types them apart), and the not-found view has no
 * page component under it at all — so there is no page to inherit the query from. `proxy.ts`
 * forwards the visitor's `?site=` as `x-omnion-site-hint`, which is readable from `headers()`
 * anywhere below, including here.
 *
 * `null` is the honest "this request names no site" (a loopback address with no `?site=`, before
 * any site exists), and it falls back to the installation theme exactly as before — so the
 * renderer still draws a complete page when the API cannot answer.
 */
export default async function RootLayout({ children }: LayoutProps) {
  const incoming = await headers();
  const theme = resolveThemeOrWarn(
    (await getSiteTheme(incoming.get(SITE_HINT_HEADER) ?? undefined)) ?? undefined,
    "this site",
  );

  return (
    <html lang="en" data-theme={theme.key} data-theme-version={theme.version}>
      <body>{children}</body>
    </html>
  );
}