/**
 * The Corporate theme.
 *
 * Theme code is presentation: it reads the content the renderer hands it and nothing else.
 * The manifest is the single source of the metadata — the version and modes below are read
 * from the file, not restated, so the gallery and the renderer can never disagree.
 */
import { defineTheme } from "@omnion/theme-sdk";

import manifest from "../omnion.theme.json";
import { corporatePageLayout } from "./page-layout";

/** The Corporate theme, as the renderer activates it. */
export const corporateTheme = defineTheme({
  key: manifest.key,
  name: manifest.name,
  version: manifest.version,
  modes: manifest.modes,
  pageTypes: manifest.pageTypes,
  PageLayout: corporatePageLayout,
});
