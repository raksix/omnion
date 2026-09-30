import { agencyTheme } from "@omnion/theme-agency";
import { commerceTheme } from "@omnion/theme-commerce";
import { corporateTheme } from "@omnion/theme-corporate";
import { documentationTheme } from "@omnion/theme-documentation";
import { governmentTheme } from "@omnion/theme-government";
import { magazineTheme } from "@omnion/theme-magazine";
import { minimalTheme } from "@omnion/theme-minimal";
import { portfolioTheme } from "@omnion/theme-portfolio";
import { startupTheme } from "@omnion/theme-startup";
import { techTheme } from "@omnion/theme-tech";
import type { SiteTheme } from "@omnion/theme-sdk";

/**
 * The theme registry of the renderer (docs/03-FRONTEND.md).
 *
 * A site's active theme resolves to the key it activates here; the gallery, the Theme Builder
 * and per-site theme settings all read this map. Later phases read the active theme from the
 * site's presentation settings instead of the environment — the resolution call does not change.
 *
 * ## Why the keys are not spread over the themes
 *
 * A theme's manifest may declare `aliases` — `magazine` answers to `editorial`, `commerce` to
 * `restaurant` — because an installation pinned to an older key must still resolve. The alias
 * map is built from the manifests rather than written by hand, so a theme that adds an alias in
 * its own file gets it here without a second edit that could be forgotten. The mapping is one
 * direction: an alias never becomes a gallery key of its own, or the same theme would appear
 * twice in the picker.
 */
const registry: Record<string, SiteTheme> = {
  [agencyTheme.key]: agencyTheme,
  [commerceTheme.key]: commerceTheme,
  [corporateTheme.key]: corporateTheme,
  [documentationTheme.key]: documentationTheme,
  [governmentTheme.key]: governmentTheme,
  [magazineTheme.key]: magazineTheme,
  [minimalTheme.key]: minimalTheme,
  [portfolioTheme.key]: portfolioTheme,
  [startupTheme.key]: startupTheme,
  [techTheme.key]: techTheme,
};

/**
 * Older or alternate keys, in `key: alias` form.
 *
 * A pnpm workspace package cannot be imported for its JSON without pulling the whole package
 * graph in, and ten `import manifest from "@omnion/theme-x/omnion.theme.json"` statements in
 * the renderer would be ten more places the gallery and the renderer can disagree. The aliases
 * are therefore declared once, here, next to the registry that consumes them.
 */
const ALIASES: Record<string, string> = {
  editorial: "magazine",
  news: "magazine",
  blog: "magazine",
  restaurant: "commerce",
  shop: "commerce",
  store: "commerce",
  docs: "documentation",
  manual: "documentation",
  saas: "tech",
  "startup-soft": "tech",
  studio: "agency",
  creative: "agency",
  business: "corporate",
  enterprise: "corporate",
  personal: "portfolio",
  freelance: "portfolio",
  launch: "startup",
  landing: "startup",
  nonprofit: "government",
  public: "government",
  civic: "government",
};

/** Key of the theme a fresh installation renders with. */
export const DEFAULT_THEME_KEY = minimalTheme.key;

/** Every key the renderer can resolve, aliases included. */
export function knownThemeKeys(): string[] {
  return [...Object.keys(registry), ...Object.keys(ALIASES)];
}

/**
 * Resolve the active theme of a site; an unknown key falls back to the default instead of
 * failing a request.
 *
 * The key comes from the site's presentation setting (`GET /api/v1/public/pages/{slug}` answers
 * with the site), so choosing a theme in the panel changes what visitors see. The environment
 * variable stays as the installation-wide fallback for a renderer that serves several sites.
 */
export function resolveTheme(
  key: string | undefined = process.env.OMNION_WEB_THEME,
): SiteTheme {
  const wanted = key?.trim().toLowerCase();
  if (wanted) {
    const resolved = ALIASES[wanted] ?? wanted;
    const theme = registry[resolved];
    if (theme) {
      return theme;
    }
  }
  return minimalTheme;
}
