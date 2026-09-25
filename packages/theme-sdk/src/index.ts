/**
 * The Omnion theme contract (docs/03-FRONTEND.md).
 *
 * A theme is presentation only: it renders the content the platform hands it and never owns
 * content of its own, so switching themes never makes a site's pages disappear. The renderer
 * (`apps/web`) resolves the active theme through keys and hands every page to
 * `SiteTheme.PageLayout`; the Theme Builder and the marketplace grow on top of this contract.
 */
import type { PublishedPage } from "@omnion/types";
import type { ComponentType } from "react";

/** What a theme receives for one page. */
export interface PageLayoutProps {
  /** The published page to render — site, page identity and the visible revision. */
  content: PublishedPage;
}

/**
 * Metadata a theme declares in its `omnion.theme.json` manifest.
 *
 * The manifest is the file the gallery, the installer and (later) the marketplace read, so it
 * stays data — never code.
 */
export interface ThemeManifest {
  /** Stable key the site activates (`minimal`). */
  key: string;
  /** Display name (`Minimal`). */
  name: string;
  /** SemVer of the theme. */
  version: string;
  /** One-line description for the gallery. */
  description: string;
  /** Author of the theme. */
  author: string;
  /** Renderer engine the theme targets. */
  engine: string;
  /** Colour modes the theme ships (`light`, `dark`). */
  modes: string[];
  /** Page content types the theme renders. */
  pageTypes: string[];
  /** Layout slots the theme implements. */
  layouts: string[];
}

/** A theme the renderer can activate. */
export interface SiteTheme {
  /** Stable key the site activates. */
  key: string;
  /** Display name. */
  name: string;
  /** SemVer of the theme. */
  version: string;
  /** Colour modes the theme ships. */
  modes: string[];
  /** Page content types the theme renders. */
  pageTypes: string[];
  /** Component that renders one page of the site. */
  PageLayout: ComponentType<PageLayoutProps>;
}

/**
 * Declare a theme.
 *
 * The helper adds no behaviour — it gives the definition site the contract's type, so a theme
 * that forgets a layout fails where it is written instead of where it renders.
 */
export function defineTheme(theme: SiteTheme): SiteTheme {
  return theme;
}
