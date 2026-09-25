# REQ-050 — First-run Onboarding / Setup Wizard

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** platform (`apps/admin` + CLI)
> **Source:** owner directive — "kurulum için onboarding ekranı falan, request olarak ekle"

## Request

A first-run onboarding + installation experience for both humans and servers.

Web wizard (first start on an empty database):

```text
Welcome
  ↓
Create owner account
  ↓
Create organization
  ↓
Create first site (domain)
  ↓
Choose theme
  ↓
(optional) Connect AI provider
  ↓
Choose starter modules
  ↓
Done
```

- Step progress indicator; each step skippable where safe (sane defaults).
- After completion: a getting-started checklist card on the dashboard
  (invite users, publish the first page, set up SMTP, connect a domain, ...).

CLI equivalent (for headless servers — tools/cli per docs/04):

```text
omnion setup      # interactive first-run
omnion doctor     # environment check: database / redis / storage / migrations / config / plugin runtime
omnion migrate    # run migrations
omnion seed       # optional demo data
```

Plus installation documentation: `docker compose up -d` quickstart (docs/02), `.env.example`
template, and a "requirements" section (Postgres, Redis, S3/MinIO).

## Acceptance criteria

- Fresh install → working admin panel in under 5 minutes, zero manual SQL.
- Wizard state is resumable after a page refresh; no dead ends.
- Owner account creation stores hashed credentials and upholds the invariants from
  docs/07-IAM.md §20 (at least one Owner must exist).
- `omnion doctor` gives a per-check pass/fail with actionable hints.
- All screens use Lucide icons — no emoji/symbol glyphs in the UI (project-wide UI rule).

## Notes

- Build phase: **P10** in [`docs/BUILD-BACKLOG.md`](../BUILD-BACKLOG.md).
- Related: REQ-044 (Package Installer — starter module selection), REQ-043 (white-label
  branding during setup), docs/02-ARCHITECTURE.md (Docker), docs/07-IAM.md (owner role).
