# REQ-084 — Theme SDK & Packaging

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `packages/theme-sdk` + CLI
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Third parties must be able to build themes.

- `omnion.theme.json` manifest (name, version, engine range, slots, settings schema, preview).
- Directory contract: layouts/, pages/, components/, blocks/, assets/, styles/, locales/.
- Theme API: site(), page(), menu(), posts(), media(), translations(), settings() — typed helpers.
- `omnion theme create` scaffolding CLI and `omnion theme validate` (CI-able).
- Package format for distribution (zip), versioning rules, compatibility matrix, changelog.

## Implementation spec

### Scope (in / out)

**In**

- Manifest v2 in `omnion.theme.json`: `key`, `name`, `version` (SemVer), `description`, `author`, `license`, `homepage`, `engines` (renderer range, e.g. `"omnion-web": ">=0.4 <1.0"`), `modes`, `pageTypes`, `slots`, `sections` (used and overridden), `settingsSchema` (a JSON Schema the site settings surface renders), `preview` (`images`, `sampleContent`), `locales`, `aliases`, `kind` (`declarative` or `code`), `parent` (optional child theme) and `changelog`. The manifest stays pure data — never code — so the gallery, the installer, the validator and the marketplace all read one file without executing anything.
- Directory contract, enforced by the validator: `layouts/`, `pages/`, `components/`, `blocks/`, `assets/`, `styles/`, `locales/` for declarative themes, plus `src/` with an entry module for code themes. Every path is relative, lower-case, and free of traversal, absolute paths and symlinks.
- A single JSON Schema at `packages/theme-sdk/schema/theme.schema.json` (versioned as `schema_version`) is the authority: the CLI validates offline with it, the API validates uploads and saves with it, and the renderer refuses a manifest it cannot validate.
- Theme API (`packages/theme-sdk`) — typed helpers available inside a theme: `site()` (handle, name, theme key, modes, site settings), `page()` (the current `PublishedPage` with revision and type), `menu()` (a named menu tree, resolved server-side), `posts()` (published list with filter, sort and pagination for list slots), `media()` (media reference to a sized variant plus the mandatory alt text), `translations()` (dictionary lookup by namespace and locale with documented fallback order), `settings()` (the site's published theme settings with the manifest defaults applied). Helpers return data already resolved by the server: a theme never talks to the database, never fetches over the network at render time, and an async helper called outside a render context throws a named error rather than returning undefined.
- Composition helpers: `defineTheme()`, `defineSection()` and `defineBlock()` with typed props, so a theme replacing a section renderer keeps the registry key and contract from REQ-083.
- CLI (`packages/cli`, package `@omnion/cli`, binary `omnion`, Node, no Rust toolchain required):
  - `omnion theme create <dir> [--template minimal|blank] [--kind declarative|code]` — scaffolds the directory contract, a valid manifest, a token stub, a sample layout, a locale file and a README with the first `validate` command.
  - `omnion theme validate <dir> [--strict] [--json] [--schema <path>]` — offline validation with exit code 0 on pass and 1 on failure, stable machine-readable findings (`path`, `severity`, `rule`, `message`, `hint`), usable in CI without network access.
  - `omnion theme pack <dir> [--out <file>]` — builds the distribution zip with a checksum file, refusing to pack a directory that fails validation.
  - `omnion theme diff <a.zip> <b.zip>` — manifest, file list, size and token differences between two packages, for reviewers.
  - `omnion theme outdated <dir>` — compares the manifest's `engines` range against the installation's accepted range (read from `GET /api/v1/themes/sdk` when reachable, from the local schema version otherwise).
- Packaging format (`.omnion-theme.zip`): manifest at the archive root; `MANIFEST.sha256` listing every file; `CHANGELOG.md` with a SemVer-ordered entry for the packed version; a `LICENSE` file; assets only in the documented extensions and under the size caps; no executable code in `kind: "declarative"` packages. The API stores the archive, records its checksum and extracts into a quarantined directory before anything is activated.
- Versioning and compatibility rules: theme SemVer is the author's own; `engines` gates installation, with the expected range shown when it fails; the compatibility matrix in `docs/themes/COMPATIBILITY.md` is maintained per renderer minor; deprecations warn for one minor before `--strict` turns them into errors; child themes may declare one `parent` and only override tokens, slots and sections (no cycles, one level deep).
- Public documentation as a deliverable: `docs/themes/AUTHORING.md` (a ten-minute tutorial from `create` to a packed theme), the manifest reference generated from the schema, the Theme API reference generated from the types, an example theme (`themes/minimal` as the worked example) and a troubleshooting page listing every validation rule with its fix.

**Out**

- Signing, review and trust flow for third-party code packages: v1 accepts declarative packages from uploads, and code themes only via the bundled/in-repo path. A trust decision for uploaded code belongs to REQ-044 / REQ-048 and must be explicit.
- Paid licensing, purchase flow, ratings, reviews, a hosted theme registry and version discovery (REQ-048 owns the marketplace).
- Alternate template languages (PHP-style includes, server-side templating DSLs), arbitrary JavaScript inside declarative packages, and remote asset loading at render time.
- A separate dev server with hot reload: `omnion theme preview` is a thin wrapper over the renderer's preview route.
- Automatic migration of theme settings between major manifest versions: a major bump may require a documented manual step, which the changelog must state.

### Screens (UI)

The CLI is the primary interface here; the admin screens this request touches are where the CLI's output surfaces:

- **`/themes/upload`** (REQ-062's screen, content defined here) — drag-and-drop or file picker for a `.omnion-theme.zip`, then a validation report: overall status (pass / warn / fail / refused), the manifest summary (key, version, author, licence, engine range, kind, slots), findings grouped by severity with `path:line`, an asset budget table and a `Download JSON` action. `Install` is enabled only on pass, and installing lands the package in a quarantined state with a visible badge until activation.
- **`/themes/packages`** — installed packages: key, version, source (bundled / uploaded), kind, engine range, checksum, validation status, installed by and at, with row actions `Details`, `Export`, `Uninstall`. Bundled rows show no uninstall action.
- **`/settings/developer`** — a Theme SDK card: schema version, CLI version, the installation's accepted engine range, a copyable `omnion theme create` line, and a link to the authoring guide and compatibility matrix.
- **CLI output as a screen** — text mode is colour-coded, one finding per line with a hint, and a summary line ("3 errors, 2 warnings — packed version 1.2.0"); `--json` emits a stable schema so CI can annotate a pull request. `omnion theme create` prints the next three commands to run.
- **States** — report states: pass, warn, fail, refused (engine mismatch, code in a declarative package), quarantined, checksum mismatch, "already installed at this version", and "newer version available" for a re-upload. Long validations show progress per stage (manifest, directory, schema, assets, runtime smoke render).
- **Keyboard** — `⌘⇧U` jumps to upload; in the report, `j` / `k` move between findings, `Enter` opens the offending file in a read-only package viewer, `Esc` returns to the list; the viewer never renders archived HTML, only text files and images.
- **Mobile** — the upload screen works on a phone (upload, read the report, install), with a note that authoring themes expects a desktop; the package viewer collapses to one column and long file paths wrap instead of scrolling horizontally.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| POST | `/api/v1/themes/validate` | Dry-run validation of a package (multipart zip or installed package id) — returns the report, installs nothing | `themes.install` |
| POST | `/api/v1/themes/install` | Install a validated package into quarantine (bundled keys refused, checksum recorded) | `themes.install` |
| GET | `/api/v1/themes/packages` | Installed packages with key, version, source, kind, engine range, status, checksum | `themes.read` |
| GET | `/api/v1/themes/packages/{id}` | One package: manifest, latest validation report, install history | `themes.read` |
| DELETE | `/api/v1/themes/packages/{id}` | Uninstall an uploaded package (never a bundled theme, never the active theme without switching first) | `themes.install` |
| GET | `/api/v1/themes/sdk` | Schema version, SDK version, accepted engine range, package limits the server enforces | `themes.read` |
| GET | `/api/v1/themes/export/{key}` | Export a site's theme configuration as a package (manifest plus tokens and layouts) | `themes.export` |

`themes.read`, `themes.install` and `themes.export` come from the theme surface defined alongside REQ-062 and are added to `crates/permissions/src/catalogue.rs`; every route is guarded with `guards::require(...)`. Package limits are server-side facts, not CLI opinions: ZIP archive ≤ 20 MiB, extracted total ≤ 80 MiB, ≤ 2 000 files, per-file ≤ 5 MiB, compression ratio ≤ 100:1, and JSON manifests ≤ 1 MiB. A rejected upload returns a report in the same shape as a dry run, so the UI renders one component for both.

### Data model

- Migration `0019_theme_packages.sql` (next free number at merge; the theme requests add migrations in merge order):
  - `theme_packages` — `id uuid primary key default gen_random_uuid()`, `key text not null check (key ~ '^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$')`, `version text not null` (SemVer checked in code), `name text not null`, `author text not null`, `license text`, `kind text not null check (kind in ('declarative','code'))`, `source text not null check (source in ('bundled','uploaded'))`, `engine_range text not null`, `manifest jsonb not null`, `checksum text not null`, `size_bytes bigint not null check (size_bytes > 0)`, `storage_key text not null`, `status text not null check (status in ('quarantined','installed','failed','removed'))`, `installed_by uuid references users (id) on delete set null`, `installed_at timestamptz not null default now()`, `validated_at timestamptz`, unique `(key, version)`, index `(key, installed_at desc)`.
  - `theme_package_validations` — `id bigserial`, `package_id uuid references theme_packages (id) on delete cascade`, `schema_version text not null`, `status text not null check (status in ('pass','warn','fail','refused'))`, `errors integer not null default 0`, `warnings integer not null default 0`, `report jsonb not null`, `created_at timestamptz not null default now()`, index `(package_id, created_at desc)`.
  - `theme_package_files` — `id bigserial`, `package_id uuid references theme_packages (id) on delete cascade`, `path text not null`, `size_bytes bigint not null`, `sha256 text not null`, unique `(package_id, path)`: the extracted inventory, used by the viewer and by uninstall cleanup.
- The archive itself lives in object storage under the `storage_key`; the database holds metadata, the inventory and every report, so the detail screen can be rebuilt without the archive.
- Removal: uninstall deletes the extracted directory, the object and the rows (`on delete cascade`), and refuses while the package backs the site's active theme — the caller must switch the theme first, which is a deliberate two-step.
- The bundled ten themes from REQ-082 are not packages: they ship in the repository and are listed from the renderer registry; the package table exists for uploaded ones and records `source = 'uploaded'`.

### Events

- `theme.package.validated` — a validation run finished (payload: key, version, status, error and warning counts, schema version, source).
- `theme.package.installed` — a package passed validation and was installed (payload: key, version, kind, engine range, checksum, actor).
- `theme.package.rejected` — validation failed or the archive was refused (payload: key if readable, version if readable, status, first three error rules, checksum).
- `theme.package.removed` — a package was uninstalled (payload: key, version, actor).
- Package installation never emits an activation event: installing and activating are separate facts, and REQ-062 owns activation.

### Acceptance criteria

- [ ] `omnion.theme.json` schema v2 exists at `packages/theme-sdk/schema/theme.schema.json`, validates the ten bundled themes (REQ-082) and rejects a fixture for each forbidden case: missing manifest, bad SemVer, unknown top-level key, engine range in an invalid form.
- [ ] `omnion theme create --template blank` and `--template minimal` scaffold a directory that passes `omnion theme validate` with exit code 0 on a machine with no network access.
- [ ] `omnion theme validate` exits 1 with a named rule for every seeded fixture: missing manifest field, unknown slot, unknown section key, unknown block type, missing `CHANGELOG.md`, missing `LICENSE`, path traversal entry, absolute path, symlink, case-colliding filenames, oversized asset, archive over the limits, checksum mismatch, engine range incompatible.
- [ ] `--strict` turns warnings into failures and both exit codes are covered by fixtures; `--json` output is stable and documented so CI can parse it.
- [ ] `omnion theme pack` refuses an invalid directory and produces an archive whose `MANIFEST.sha256` verifies after a round trip; `omnion theme diff` reports manifest, file and size changes between two archives.
- [ ] The Theme API helpers are typed and usable: `site()`, `page()`, `menu()`, `posts()`, `media()`, `translations()` and `settings()` each have tests, and `media()` refuses a reference without alt text at build time.
- [ ] `settings()` applies manifest defaults over the site's published settings, and `translations()` falls back in the documented order (requested locale → site default → manifest default) with a test per step.
- [ ] A theme calling an async helper outside a render context fails with a named error, and a theme attempting a network fetch at render time fails the validation's runtime smoke render.
- [ ] Upload → validate → quarantine → install → list → uninstall works end-to-end through the API with `theme.package.*` events delivered to a subscribed webhook receiver exactly once each.
- [ ] The upload report renders pass / warn / fail / refused with `path:line` findings, an asset budget table and a JSON download; `Install` is disabled unless the status is pass.
- [ ] Engine ranges enforce: a package requiring a range the installation does not satisfy is refused with the expected range in the message, and its row records `status = 'refused'`.
- [ ] Uninstall refuses a bundled theme and refuses the active theme with the reason, and succeeds after the site switches away.
- [ ] Object storage and the extracted directory are cleaned on uninstall; no orphaned archive remains (verified by listing storage and the extract root before and after).
- [ ] ZIP handling is hostile-input safe: a bomb-shaped archive, a traversal entry, an absolute path and a symlink are all refused before extraction, and the extractor stays inside its temp root.
- [ ] Child themes: one `parent` level is allowed, cycles are refused, overrides merge tokens and slots predictably, and the parent must be installed for the child to activate.
- [ ] `docs/themes/AUTHORING.md`, the manifest reference and the Theme API reference are published, and a reader can go from `create` to a packed theme using only those pages.
- [ ] `docs/themes/COMPATIBILITY.md` states the supported engine ranges per renderer minor, and a deprecation warns for one minor before `--strict` fails it.
- [ ] `/themes/packages` and the Theme SDK card on `/settings/developer` render installed packages and SDK facts; both are covered by the walkthrough.

### QA plan

- **Walkthrough must click:** `/themes/upload` with a valid package fixture (pass), then fixtures for warn and fail (read the findings, check that Install stays disabled while failing), then an engine-mismatch fixture (refused, expected range shown); install a valid package; open `/themes/packages`, check the badge and detail; export a site's theme; uninstall the package after switching the active theme; open `/settings/developer` and copy the CLI line. Outside the browser, run the CLI on the same fixtures and compare the exit codes and JSON with the API reports.
- **Visual check should see:** the report readable at a glance (severity, path, hint), the quarantine badge unmistakable, disabled `Install` visually disabled and not merely inert, the package viewer showing text and images but never executing anything, and the SDK card showing accurate versions and ranges.

### Slices

1. **Schema, manifest v2 and typed API** — schema file with versioning, v2 manifest type and validation in `packages/theme-sdk`, the seven API helpers with tests and context guards, and the ten bundled manifests upgraded (coordinated with REQ-082).
   *Done when:* the bundled themes validate against schema v2 and helper tests cover each helper's fallback path.
2. **CLI** — `packages/cli` with `create`, `validate`, `pack`, `diff` and `outdated`; offline validation; text and JSON output; a fixture per validation rule; the authoring guide and reference pages.
   *Done when:* every fixture produces the documented exit code and rule name, and a reader follows the tutorial to a packed theme.
3. **Install pipeline and UI** — `theme_packages`, `theme_package_validations` and `theme_package_files`; validate/install/list/detail/uninstall/export endpoints; quarantine semantics; checksum and engine enforcement; the upload report, packages list and SDK card.
   *Done when:* a package goes from upload to active theme through the API, and uninstall cleans storage and refuses the active theme.
4. **Compatibility and child themes** — `docs/themes/COMPATIBILITY.md`, deprecation mechanics with warnings then strict failures, `parent` merge rules with cycle refusal, walkthrough coverage of the new screens and the runtime smoke render in validation.
   *Done when:* the matrix matches the implemented ranges, a deprecated fixture warns then fails under `--strict`, and a child theme renders with a single override layer.

### Risks / notes

- The schema is a public contract: version it, keep validation messages and rule names stable, and treat a message change as a breaking change for CI consumers.
- Uploads are hostile input. The extractor works in a temp root with quotas, rejects traversal, absolute paths, symlinks and case collisions, and caps the compression ratio before writing anything to disk.
- Quarantine is the safety property: an installed-but-unactivated package must not be able to influence a live site, and the renderer resolves uploaded keys only when they are active and installed.
- "No code in declarative packages" is a security decision, not a temporary gap; relaxing it requires the marketplace trust flow and an explicit installation setting, both documented.
- One validator or none: the CLI and the server must share the schema and the rule set, because a second implementation drifts within a release and authors stop trusting the tool.
- Child themes multiply loopholes: one parent level, no cycles, and overrides limited to tokens, slots and sections — anything more becomes a fork with extra steps.
- Authors judge the SDK by its documentation, not its code: the tutorial, the reference pages and the worked example theme are part of the deliverable, and a stale reference is a bug.
- Manifest v2 must stay additive over v1 where possible so existing single-theme installations upgrade without a manual edit; a missing field gets a documented default rather than a validation error.
