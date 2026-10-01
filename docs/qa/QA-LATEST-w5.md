# Omnion QA — latest pass (w5)

- When: 2026-10-01T11:56:22.348Z · artifacts: `../../../dev/shm/qa-w5-t100c/20261001-105809`
- Interactions: 1262 clicks · 123 field fills · 36 form submissions · 1293 screenshots
- Console errors: 139 · failed requests: 137 · dialogs: 0
- Programmatic findings: 293 (high 281 · medium 9 · low 3)
- Vision issues: skipped (no vision API key found (env VISION_MCP_API_KEY or config.yaml))

## Top findings

- **[high] clone-reported-done-without-copying-the-pages** — job=done copied=0 production=0
- **[high] promotions-tab-absent** — undefined
- **[high] archived-environment-not-under-the-archived-filter** — the environment archived a moment ago does not appear under ?status=archived, so the one control that gets it back does not work
- **[high] env-promotionApplied** — an approved promotion did not reach a terminal state in the database — the dialog wrote a record the apply path never finished
- **[high] env-archivedVisibleUnderFilter** — the archived environment does not appear under ?status=archived
- **[high] env-changes-tab-empty-after-an-edit** — the staging environment holds an edited page and the Changes tab lists none — the diff an operator would promote is missing
- **[high] env-not-exactly-one-production** — an organization must hold exactly one production environment; the list reports a different number
- **[high] maintenance-banner-clears** — the banner is still up one poll interval after the window was closed; writes work again but the panel keeps claiming they do not
- **[high] purge-malformedMessage** — a target with no leading slash was accepted with no message naming the problem
- **[medium] purge-pageOneOfMany** — the history holds more purges than the API's total reports, so the count is stale or the query is not paging
- **[high] purge-drawerError** — the drawer shows no provider error for a purge that failed
- **[medium] purge-retryLabel** — the retry control does not name how many targets it will re-send — a 'Retry' on a partial row hides whether already-delivered targets are re-sent
- **[high] depth-pass-failed** — the "mediaFiles" depth pass did not complete: the media browser did not render
- **[high] depth-pass-failed** — the "mediaFileDetail" depth pass did not complete: no file to open — the upload step did not succeed
- **[high] depth-pass-failed** — the "mediaPresets" depth pass did not complete: the presets screen did not render
- **[high] depth-pass-failed** — the "mediaStorage" depth pass did not complete: the storage tab did not render
- **[high] depth-pass-failed** — the "mediaShares" depth pass did not complete: no file to share — the upload step did not succeed
- **[high] depth-pass-failed** — the "mediaGrants" depth pass did not complete: the library rendered no file to reach the permissions tab
- **[high] depth-pass-failed** — the "mediaDuplicates" depth pass did not complete: no file input — the duplicate pair was not uploaded
- **[high] depth-pass-failed** — the "backups" depth pass did not complete: the backup overview did not render
- **[high] depth-pass-failed** — the "mediaRetention" depth pass did not complete: the retention tab did not render
- **[high] depth-pass-failed** — the "environments" depth pass did not complete: it returned no reason
- **[high] depth-pass-failed** — the "health" depth pass did not complete: /health did not finish loading
- **[high] broken-image** — media-settings: http://127.0.0.1:3104/api/v1/media/54af1a1f-9b33-4622-b184-b68af76e777c/raw?preset=standard
- **[high] console-error** — main http://127.0.0.1:3104/media/settings: Failed to load resource: the server responded with a status of 422 (Unprocessable Entity)
