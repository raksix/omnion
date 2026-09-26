# Deployment — live development instance

`omnion.fermag.com.tr` runs the platform's development deployment on the shared server:

| Piece | Where |
|---|---|
| Admin panel | `https://omnion.fermag.com.tr` → pm2 `omnion-admin` (127.0.0.1:3180) |
| Platform API | nginx `/api/*`, `/healthz`, `/readyz` → pm2 `omnion-api` (127.0.0.1:4180) |
| Public renderer | `https://demo.omnion.fermag.com.tr` → pm2 `omnion-web` (127.0.0.1:3280) |
| Database | PostgreSQL `omnion_live` (the development stack's `postgres:5433` container) |
| Redis | the development stack's `redis:6380` |
| Source tree | a detached git worktree at `/mnt/apopic/omnion-live`, pinned to `origin/main` |
| Environment file | `/root/.omnion-live.env` (0600, outside the repository — never commit it) |
| TLS | Let's Encrypt via certbot (`/etc/letsencrypt/live/omnion.fermag.com.tr/`), auto-renewing |

## Deploy a new revision

```bash
bash infra/deploy/deploy-omnion-live.sh
```

The script: pulls `origin/main` into the worktree, builds the API (release) and both web apps,
restarts the three pm2 processes and prints the health of each. It is idempotent.

Notes:

- The panel's `/api/*` proxy is resolved by **nginx**, not by Next's rewrite manifest (Next bakes
  `rewrites()` at build time, which makes a build-time API origin sticky). The build still passes
  `OMNION_API_URL` so server-side code uses the same origin.
- pm2 processes are recreated (delete + start) so environment changes always apply; `pm2 save`
  keeps the list across reboots.
- The QA harness (`scripts/qa/run.sh`) is completely separate: it owns the disposable
  `omnion_qa` database and ports 18080/3100/3200 and never touches the live instance.
