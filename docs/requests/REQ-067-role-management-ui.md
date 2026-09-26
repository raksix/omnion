# REQ-067 — Role Management UI

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** admin (`apps/admin`) + core (`crates/permissions`)
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

The screen that makes the permission system usable.

- Role registry list: name, description, colour, icon, member count, scope badges, search.
- Create/edit role form with a grouped permission picker (allow / deny toggles, module groups).
- Preset roles shipped: Super Admin, Editor, SEO Manager, Support, Viewer, Employee.
- Role hierarchy: drag-order priority, hierarchy-protected changes (cannot outrank yourself).
- Role inheritance (derived roles extend a parent), multi-level chains, effective diff view.
- Role versioning and audit timeline: who changed what, diff between versions.
- Deletion guards: self-lockout prevention, owner/administrator existence invariants.
