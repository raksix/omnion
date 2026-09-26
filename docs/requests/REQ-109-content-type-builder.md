# REQ-109 — Content Type Builder

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core (`crates/content`) + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

User-defined content models, not just pages.

- Content type registry with fields: text, rich text, number, date, boolean, image, file, relation, select, JSON.
- Built-in types shipped: Page, Blog Post, Product, Employee, Event — as editable starting points.
- Field options: required, unique, default, min/max, localisation per field.
- Relations between types (one-to-many, many-to-many) with referential integrity.
- Generated REST endpoints + panel screens + API docs per type; migrations generated safely.
- Validation rules and a preview of the JSON schema the API exposes.
