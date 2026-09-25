/**
 * The Minimal theme.
 *
 * Theme code is presentation: it reads the content the renderer hands it and nothing else.
 * The layout lives in `page-layout.tsx`, the stylesheet in `styles/minimal.css` and the
 * identity in `omnion.theme.json` — the manifest stays the single source of the metadata.
 */
import { defineTheme } from "@omnion/theme-sdk";

import manifest from "../omnion.theme.json";
import { MinimalPageLayout } from "./page-layout";

/** The Minimal theme, as the renderer activates it. */
export const minimalTheme = defineTheme({
  key: manifest.key,
  name: manifest.name,
  version: manifest.version,
  modes: manifest.modes,
  pageTypes: manifest.pageTypes,
  PageLayout: MinimalPageLayout,
});
