# REQ-131 — CLI & Generators

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `apps/cli` + `tools/`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Everything a developer needs from the terminal.

- `omnion dev|build|test|migrate|seed|doctor` — no hand-run Docker commands for the common path.
- `omnion plugin create` / `omnion theme create` scaffolds with CI-ready tests and docs stubs.
- `omnion doctor` verifies environment, database, redis, storage, migrations and AI providers.
- `omnion backup create|restore|verify` wrapping the backup centre (REQ-013).
- Machine-readable output (`--json`) for scripts and CI.

## Implementation spec

> **Band:** migration `0131` reserved; append-only ledger — take the next free number if taken · **App:** `apps/cli` (the `omnion` binary) · **Tools:** `tools/generators` · **Server touchpoints:** REQ-013 backup centre, REQ-014 system health, REQ-033 developer platform, REQ-097 AI provider runtime.

### Scope (in / out)

**In**

- One entry point: `omnion` with subcommands `init`, `dev`, `build`, `test`, `migrate`, `seed`, `doctor`, `plugin`, `theme`, `backup`, `login`, `logout`, `link`, `open`, `version`, `completion`. Help exists at every level, and an unknown flag fails fast naming the closest known command.
- `omnion dev`: starts the development stack through the repository's own compose file (database, cache, object storage, api, admin) with health-gated readiness — the admin URL is printed only after every service is healthy; interleaved logs are prefixed by service; flags `--only api,admin`, `--profile`, `--reset` (confirmed) and default migrations plus minimal seed, so a fresh checkout reaches a usable panel without a single hand-run container command.
- `omnion build` and `omnion test`: delegate to the same workspace and crate task definitions CI runs, never a parallel reimplementation; filters (`--filter`, `--affected`) and `--watch`; failing tasks propagate their exit code; output stays parseable when `--json` is set.
- `omnion migrate`: `status` (ledger, applied versus pending, checksum state), `up` (applies pending and refuses on drift, naming the offending file), `new <name>` (allocates the next free number from the ledger) and `verify` (checksums plus the rehearsal hook where the environment supports it). Refusal rules come from the same ledger checks as REQ-129.
- `omnion seed`: fixture sets (`minimal`, `demo`, `full`), idempotent and safe to re-run, refusing against a non-development database unless `--force` names the environment explicitly.
- `omnion doctor`: a check engine with stable check ids covering toolchain versions, port availability, environment completeness against `.env.example`, database connectivity and migration ledger, cache ping, object-storage probe (create, read, delete a probe object), optional mail reachability, AI provider reachability through the provider runtime with a token-free capability ping (REQ-097), disk and inode space, clock skew, compose file validity and container runtime status. Each check prints pass/warn/fail with a fix hint; exit code 1 when any check fails, 0 with warnings present.
- Generators: `omnion plugin create`, `omnion theme create` and `omnion module create` prompt with sensible defaults and validate name and slug. The generated project contains a manifest, an entry point, one working sample (a route, a block or a node), unit tests wired into CI, a docs stub, a changelog, a README describing the dev loop and a workflow file. A linked instance records a scaffold audit row.
- `omnion backup create|restore|verify`: wraps the backup centre API (REQ-013). `create` polls to completion and can download the artifact; `verify` checks manifest and checksums; `restore` demands a typed confirmation, refuses while another restore holds the lock, defaults to a dry run against a scratch database and reports success only after the restored instance passes a smoke check.
- `--json` contract: a stable envelope (`ok`, `command`, `data`, `warnings`, `error { code, message, hint }`) with documented error codes; data on stdout, progress on stderr, so `omnion doctor --json | jq` never breaks; `NO_COLOR` honoured; humans get tables.
- Configuration and credentials: resolution order flags > `OMNION_*` environment > repository `omnion.toml` > user config. Credentials live in the OS keychain with a documented 0600 file fallback behind an explicit flag and a warning. `omnion login` runs the device-code flow and prints the granted scopes; `omnion link` binds the working directory to an instance; the CLI never writes a secret into the repository.
- Distribution: one static binary per platform plus an npm wrapper and a container image for CI; `omnion version --check` compares against the release channel; the CLI refuses to talk to a server whose minimum client version is newer than itself, with an upgrade hint instead of a parse error.

**Out**

- A second source of truth for build steps: the CLI reads the same compose files, task definitions and migration ledger CI uses.
- The CLI never provisions servers or deploys applications (REQ-128 owns deployment tooling), never runs GUI or IDE integrations, and sends no telemetry — its only network calls go to the linked instance and, on explicit `--check`, the release channel.
- Native (containerless) hosting of the whole stack is a documented limitation of this wave, not a goal.

### Screens (UI)

This REQ ships no admin screens of its own. The browser touchpoints it depends on are:

- Device-code approval stays on `/developer/cli` (REQ-033) and must show the requesting host and OS hint the CLI sends; approving with an account lacking key-management permission is refused.
- Backup runs started from the CLI appear in the backup centre (REQ-013) with a `source: cli` label, the initiating user and the paired device name, so an operator can see who triggered what.
- `omnion open` deep-links into the admin and never prints a URL carrying a token or session identifier; the login flow hands off to the browser and the CLI waits for approval only.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| POST | `/api/v1/cli/device-code` | Start a device-code login | — (public, rate-limited) |
| POST | `/api/v1/cli/device-code/approve` | Approve a device code from a browser session | `developer.keys.manage` |
| POST | `/api/v1/cli/token/refresh` | Refresh a CLI device token without re-authentication | paired device token |
| GET | `/api/v1/cli/devices` | List paired devices for the caller | `developer.keys.read` |
| DELETE | `/api/v1/cli/devices/{id}` | Revoke a device and its tokens | `developer.keys.manage` |
| GET | `/api/v1/system/health` | `doctor` and `dev` readiness probe | public, minimal detail |
| GET | `/api/v1/system/version` | Server version and build metadata for skew checks | public |
| GET | `/api/v1/migrations/status` | Ledger state for `migrate status` | `settings.manage` |
| POST | `/api/v1/backups` | Create a backup | `backups.manage` |
| GET | `/api/v1/backups/{id}` | Poll a backup or restore job | `backups.read` |
| POST | `/api/v1/backups/{id}/verify` | Verify manifest and checksums | `backups.manage` |
| POST | `/api/v1/backups/{id}/restore` | Request a restore (typed confirmation, single lock) | `backups.restore` |

The `/api/v1/cli/*` routes are new in this REQ; health, version, migration and backup routes belong to REQ-014, REQ-129 and REQ-013 and are reused rather than redefined. Tokens are issued only after browser approval, are scoped, expire, and are stored hashed — the plaintext exists once, on the machine that requested it.

### Data model

Migration `0131_cli_and_generators.sql`.

```sql
cli_devices (id uuid pk, user_id uuid -> users, name text, hostname text, os text, arch text, cli_version text,
  token_hash text unique, scopes jsonb default '[]', last_seen_at timestamptz, created_at timestamptz, revoked_at timestamptz)
  index (user_id, revoked_at)
cli_device_codes (id uuid pk, code_hash text, user_code text unique, user_id uuid null -> users,
  requested_scopes jsonb default '[]', device_name text, expires_at timestamptz, approved_at timestamptz,
  consumed_at timestamptz, created_at timestamptz)  index (expires_at) where consumed_at is null
cli_scaffold_runs (id uuid pk, kind text in ('plugin','theme','module'), slug text, template_version text, files int,
  performed_by uuid -> users, cli_version text, created_at timestamptz)  -- audit only, no file contents
cli_token_events (id bigserial pk, device_id uuid null -> cli_devices, user_id uuid null -> users,
  kind text in ('issued','refreshed','revoked','expired'), created_at timestamptz)  -- 30-day retention
```

Notes: device tokens are hashed exactly like API keys (REQ-033) and carry scopes, so a pair of lost laptops cannot widen access. Device codes are single-use, short-lived and rate-limited per address. Scaffold runs record metadata only — no generated file ever leaves the developer's machine. `cli_token_events` feeds the device list's activity column and is pruned on the same schedule as request logs.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `cli.device.paired` · `.revoked` | Device lifecycle | `device_id`, `user_id`, `name`, `os` |
| `cli.token.refreshed` · `.expired` | Session events | `device_id`, `user_id` |
| `sdk.scaffold.generated` | A linked scaffold generation (the event REQ-033 defines, reused) | `kind`, `slug`, `template_version` |
| `cli.doctor.shared` | Opt-in sharing of a redacted doctor report | `check_failures`, `cli_version` (no host names, no environment values) |

Consumed: `backup.completed` · `.failed` (REQ-013) so `omnion backup create` finishes deterministically instead of polling blindly; `updates.applied` (REQ-078) invalidates the version check cache. Webhook relevance: device pairing and revocation are exactly what a security-conscious organization subscribes to; no CLI payload ever carries a token, an environment value or file content.

### Acceptance criteria

- [ ] On a clean checkout with the toolchain and container runtime installed, `omnion dev` boots database, cache, storage, api and admin and prints the admin URL when health passes — with no hand-run container command.
- [ ] `omnion dev --only api` starts only the API and its dependencies; `--reset` refuses without confirmation.
- [ ] `omnion build` and `omnion test` delegate to the CI task definitions; a failing test returns a non-zero exit code.
- [ ] `omnion migrate up` applies pending migrations, `status` shows the ledger, and `new <name>` allocates the next free number and refuses a duplicate name.
- [ ] `omnion migrate up` refuses on a checksum mismatch and names the offending file.
- [ ] `omnion seed --minimal` run twice leaves row counts unchanged.
- [ ] `omnion doctor` reports each check with pass/warn/fail and a fix hint, exits 1 on any failure, and 0 with warnings plus a visible summary line.
- [ ] Doctor detects, one scenario each against real breakage: a stopped database, an occupied port, a missing environment key, a migration checksum mismatch and an unreachable AI provider.
- [ ] `omnion doctor --json` output parses as JSON and contains no secrets — environment values appear as names only.
- [ ] `omnion plugin create` and `omnion theme create` produce projects that install or load from a clean checkout, with generated tests passing in CI on first push.
- [ ] Generated scaffolds include the README dev loop, changelog, docs stub and workflow file, and contain no real token or internal hostname.
- [ ] `omnion backup create` produces an artifact listed in the backup centre attributed to the CLI user; `verify` fails when a file is corrupted (proved by flipping one byte).
- [ ] `omnion backup restore` refuses without typed confirmation, refuses while another restore holds the lock, and reports success only after the restore job reaches `done`.
- [ ] `omnion login` completes the device-code flow and stores the token in the OS keychain; `omnion logout` removes it; a revoked device fails the next call with a clear message.
- [ ] Every command supports `--json` with the documented envelope and error codes, and progress never contaminates stdout.
- [ ] The binary runs on Linux, macOS and Windows (paths, line endings, keychain fallback), verified in the CI matrix.

### QA plan

The walkthrough runs the CLI inside a clean container from the repository checkout: `omnion doctor` on a healthy stack, then four induced failures (database stopped, port occupied, environment key removed, migration checksum flipped) each expecting the right hint; `omnion dev` to healthy and then `--only api`; `migrate status|up|new`; `seed --minimal` twice; `plugin create` followed by building and testing the generated project in CI; `backup create|verify` and a restore into a scratch database including the one-byte corruption case; `login` from a second terminal with browser approval, then `logout`; finally every command re-run with `--json` piped through a JSON validator. Visual check: colour disabled under `NO_COLOR`, tables render in an 80-column terminal, spinners are suppressed when not a TTY, and piped output is clean.

### Slices

1. **Command kernel and dev loop.** Parser, help, config resolution, `--json` envelope, exit codes, and delegation to compose and workspace tasks for `dev|build|test`. *Done when:* acceptance 1–3 and 15 pass in CI.
2. **Migrate, seed and doctor.** Ledger operations, fixture sets, the check engine with hints and the machine-readable report. *Done when:* acceptance 4–9 pass, including the four induced-failure scenarios.
3. **Generators.** `plugin|theme|module create` with tests, docs stub, workflow file and the server-side scaffold audit. *Done when:* acceptance 10–11 pass and a generated project passes CI on first push.
4. **Backup, login and distribution.** REQ-013 client surface, device-code login with keychain storage, cross-platform matrix, packaging and version check. *Done when:* acceptance 12–14 and 16 pass.

### Risks / notes

- Wrapper rot is the long-term risk: the CLI must consume the same compose and task files CI uses, and a CI job must exercise the CLI path rather than a handwritten equivalent, or the two drift apart silently.
- `doctor` shapes trust: false failures teach people to ignore it, so each check must be verified against real breakage in tests and warnings must stay visually distinct from failures.
- Restore is destructive: single lock, typed confirmation, scratch-first dry run, and an audit row every time — plus a smoke check before success is reported.
- On headless systems there is no OS keychain, so the 0600 file fallback is explicit (`--store=file`), warned about on use, and excluded from the repository by a generated ignore rule.
- Scaffold output lands in public repositories: generated files must contain placeholder-only examples, and a test scans generator output for token-like strings and internal hostnames.
- The `--json` envelope is a public contract: additive changes only, error codes documented and never repurposed, and a version marker in the envelope so scripts can branch.
- Version skew between a new CLI and an older server must resolve to an actionable message in both directions, never a confusing deserialization error.
- The npm wrapper and container image must install the matching binary version; a mismatch check runs at startup and reports the pin.
