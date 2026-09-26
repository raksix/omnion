> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** platform (`apps/admin` + `crates/content`)
> **Source:** owner brief — business suite / frontend depth (docs/08-BUSINESS-SUITE.md, docs/03-FRONTEND.md)

## Request

Blocks and the visual page builder.

- **Block registry**: server-defined block schemas (heading, text, image, gallery, video, CTA, columns, card grid, pricing table, testimonial, FAQ, form, embed, raw HTML, product grid, blog list) with typed props.
- **Block editor**: insert, reorder (drag), duplicate, delete, per-block settings panel, nested containers (columns with child blocks).
- **Patterns & templates**: reusable block groups and full page templates (landing, about, pricing, blog post, contact).
- **Content model**: blocks stored as typed JSON in revisions (same versioning as pages), rendered by `apps/web` through the theme engine.
- **Inline editing** on the front-end preview (click text to edit, save as draft revision).
- **Accessibility & responsiveness**: per-block viewport settings (hide on mobile/desktop), semantic HTML output.
- **Events**: `content.blocks.updated`, `content.page.published`.
