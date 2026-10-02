/**
 * The Magazine theme.
 *
 * Theme code is presentation: it reads the content the renderer hands it and nothing else.
 * The manifest is the single source of the metadata — the version and modes below are read
 * from the file, not restated, so the gallery and the renderer can never disagree.
 */
import { defineTheme } from "@omnion/theme-sdk";

import manifest from "../omnion.theme.json";
import { magazinePageLayout } from "./page-layout";

/** The Magazine theme, as the renderer activates it. */
export const magazineTheme = defineTheme({
  key: manifest.key,
  name: manifest.name,
  version: manifest.version,
  modes: manifest.modes,
  pageTypes: manifest.pageTypes,
  PageLayout: magazinePageLayout,
});
