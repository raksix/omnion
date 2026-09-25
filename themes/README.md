# Themes

Official themes of the Omnion public frontend (docs/03-FRONTEND.md). A theme is **presentation
only**: it renders the content the platform hands it and owns no content of its own, so
switching a site's theme never makes its pages disappear.

## Layout of a theme

```text
themes/<key>/
├── omnion.theme.json   # manifest — the identity the gallery and the marketplace read
├── package.json        # workspace package `@omnion/theme-<key>`
├── src/
│   ├── index.ts        # public surface of the theme
│   ├── theme.ts        # `defineTheme({ … })` — manifest + layout wiring
│   └── page-layout.tsx # the page renderer
└── styles/
    └── <key>.css       # the theme owns canvas, type scale and light/dark
```

## The contract

The renderer (`apps/web`) resolves the active theme by key and renders every page through it:

```ts
import type { PageLayoutProps, SiteTheme } from "@omnion/theme-sdk";

export function PageLayout({ content }: PageLayoutProps) {
  const { site, page, revision } = content;
  // …
}
```

- `SiteTheme` carries the manifest metadata plus `PageLayout`.
- `PageLayoutProps.content` is a `PublishedPage` from `@omnion/types` — the same shape the
  public API (`GET /api/v1/public/pages/{slug}`) answers with.
- A theme declares the colour modes and page types it ships in its manifest; the renderer sets
  `data-theme`/`data-theme-version` on `<html>` so themes and tooling can hook into them.

## Shipped today

| Key | Name | Modes | Page types |
|---|---|---|---|
| `minimal` | Minimal | light, dark | page |

The remaining nine themes of the default set (Corporate, Tech, Agency, Portfolio, Magazine,
Commerce, Documentation, Startup, Government) arrive in later phases; the gallery, the visual
Theme Builder and `omnion create-theme` all build on the contract above.
