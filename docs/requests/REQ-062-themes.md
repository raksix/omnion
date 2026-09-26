> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** platform (`themes/*` + admin)
> **Source:** owner brief — business suite / frontend depth (docs/08-BUSINESS-SUITE.md, docs/03-FRONTEND.md)

## Request

The theme system made real (docs/03-FRONTEND.md).

- **Ten default themes** shipped in-repo (minimal, editorial, corporate, portfolio, magazine, restaurant, saas, nonprofit, agency, docs) — each with tokens, layouts, and sample content.
- **Theme settings UI**: colors, typography scale, radius, spacing, logo/favicon, header/footer variants, dark mode support — persisted per site.
- **Theme Builder**: visual editing of layouts (header, footer, blog list, single page, product) using the block system; live preview; export/import theme package.
- **Theme SDK**: documented contract for theme packages (manifest, tokens, blocks, slots) + scaffolding CLI.
- **Per-site activation** with preview before publish and one-click rollback.
- **Marketplace-ready**: theme package format for REQ-048.
