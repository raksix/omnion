import { minimalTheme } from "@omnion/theme-minimal";
import type { SiteTheme } from "@omnion/theme-sdk";

/**
 * The theme registry of the renderer (docs/03-FRONTEND.md).
 *
 * A site's active theme resolves to the key it activates here; the gallery, the Theme Builder
 * and per-site theme settings all read this map. Later phases read the active theme from the
 * site's presentation settings instead of the environment — the resolution call does not change.
 */
const registry: Record<string, SiteTheme> = {
  [minimalTheme.key]: minimalTheme,
};

/** Key of the theme a fresh installation renders with. */
export const DEFAULT_THEME_KEY = minimalTheme.key;

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
    const theme = registry[wanted];
    if (theme) {
      return theme;
    }
  }
  return minimalTheme;
}
