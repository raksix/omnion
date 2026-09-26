# REQ-110 — Editorial Workflow & Content Lifecycle

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core (`crates/content`) + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Draft → review → translate → approve → published.

- Lifecycle states per content item, configurable per content type and per site.
- Review and approval steps with assignees; comments on a revision.
- Publish/unpublish, scheduled publish, scheduled unpublish, scheduled delete, scheduled theme change.
- Draft preview links (REQ-018) tied to the lifecycle state.
- Editorial calendar view: what publishes when, who owns it, conflicts highlighted.

## Implementation spec

### Scope (in / out)

**In**
- Workflow definitions: one machine per content type per site, with a version, a name, and an ordered list of states. A definition is editable while at most a handful of items sit in it; once items are in flight, states can be added but not silently removed or renamed (a state can be deprecated, and items are moved out first).
- States: key, label, colour, position, `category` (`working`, `review`, `ready`, `live`, `closed`), `is_initial`, `is_terminal`, `previewable`. The default definition for every type is Draft (`working`) → In review (`review`) → Approved (`ready`) → Published (`live`), plus Archived (`closed`).
- Transitions: from state, to state, required permission, optional required role, optional "assignee required" flag, optional approval gate (delegating to the approval engine in REQ-059 when installed, otherwise a simple two-person rule), optional due days, and an optional comment requirement.
- Assignment: any state change can set an assignee (a user or a role). An assignee or a role holder sees the item in a personal work queue with the state's due date and an overdue marker.
- Comments on a revision: threaded to two levels, resolve/unresolve, `@`-mention of panel users with notification through the platform notification path, and per-comment links back to the exact revision.
- Publish actions: publish now, unpublish now, and schedules for publish, unpublish, delete and theme change (theme change swaps the site theme at a date-time and keeps a revert entry). Schedules are timezone-aware and visible as items in the same queue as scheduled publishing.
- Draft preview links: a tokenised URL served by the preview surface (REQ-018) that renders the item's current working revision. Tokens are created from a `previewable` state, carry an expiry, are revoked when the item leaves the workflow to `live` or `closed`, and never expose the item to a signed-out visitor without the token.
- Editorial calendar: month and week views over all four schedule kinds plus in-flight items by due date, filterable by type, site, owner and category, with conflict highlighting (two schedules inside the same minute for one site, an item scheduled while it sits in a state that cannot reach `live`, a theme change overlapping a publish, and a scheduled item whose assignee is deactivated).
- Lifecycle state is per revision-aware: transitions apply to the item, the working revision is what reviewers see, and a transition to `live` publishes the revision that was approved, never a newer untracked draft.

**Out**
- The generic approval engine internals (REQ-059) — this REQ asks it for decisions and stores the reference, it does not reimplement chains, escalations or delegation.
- Automation rules and journeys (REQ-003, REQ-060): they may react to workflow events, but the machine stays deterministic.
- Channel delivery (REQ-021): reminders and mentions emit the standard notification event.
- Full project planning: no sub-tasks, sprints, story points or gantt; the calendar shows content dates only.
- Translation management depth (REQ-020/REQ-114): a `translate` category state exists in the machine, but translation memory, per-field translation UI and locale coverage rules live in those REQs.

### Screens (UI)

| Route | Screen |
|---|---|
| `/editorial/workflow` | Workflow definitions per type and site |
| `/editorial/workflow/<type>/<site>` | State machine editor |
| `/editorial/my-work` | Reviews and assignments for the signed-in user |
| `/editorial/calendar` | Calendar of schedules and in-flight items |
| `/editorial/settings` | Reminders, default due days, notification toggles |
| `/pages/<id>` (Workflow tab) | Timeline, transition actions, assignee, comments |
| `/pages/<id>` (Preview panel) | Preview links with expiry and revocation state |

- **State machine editor.** States render as ordered columns across the width (or stacked on mobile); each column lists its incoming transitions as arrows with a guard summary (`permission · role · assignee · approval · due`). Clicking an arrow opens the guard drawer: from, to, permission, role, assignee required, approval gate (on/off with the approver role), due days, comment required, notification toggle. Toolbar: `Add state`, `Save draft`, `Publish machine`. Validation refuses a machine with no initial state, two initials, an unreachable state, or a `live` state with no transition into it. A warning appears when a state is deprecated while items still sit in it, with `Move items to…`.
- **Workflow tab on a content item.** Horizontal timeline of visited states with actor and timestamp per step; below it the current state card with `Move to…` listing only the transitions allowed for the signed-in user (guarded ones are disabled with the reason shown). Right rail: assignee picker (user or role), due date, approval status when gated, and preview link panel. Comments pane at the bottom: revision-anchored threads with resolve, mention autocomplete and a link that opens the diff for the revision a comment sits on.
- **My work.** Two tabs: `Assigned to me` and `Waiting on my review`. Columns: Title, Type, Site, State, Assignee, Due, Age, Flags (`overdue`, `no assignee`, `approval pending`). Row opens the item's Workflow tab. Bulk: `Reassign`, `Remind`, and `Move to…` when one transition is legal for the whole selection.
- **Calendar.** Month/week toggle, one row per site in "all sites" mode, items coloured by state category and shaped by kind (publish, unpublish, delete, theme change, due date). A conflict summary strip sits above the grid with counts per class and a link to filter to them. Clicking an item opens a popover with the full detail and `Reschedule`, `Cancel`, `Publish now`. Drag to reschedule with a confirmation naming the new time in the site timezone.
- **Settings.** Default due days per category, reminder lead time, reminder recipient rule (assignee, role, owner), mention notifications on/off, and the two-person rule toggle with its permitted approver role.
- **States and keys.** Empty calendar shows the next seven days with a hint to schedule from the item. Keys: `g w` workflow, `g m` my work, `g e` calendar, `t` opens `Move to…`, `j`/`k` rows, `Esc` closes drawers. Every list has skeleton loading, an error strip with retry, and a real empty state.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET · POST | `/api/v1/editorial/workflows` | List definitions · create a machine for a type and site | `content.workflow.read` · `content.workflow.manage` |
| GET · PUT · DELETE | `/api/v1/editorial/workflows/{id}` | Read · save states and transitions · delete (empty only) | `content.workflow.read` · `content.workflow.manage` |
| POST | `/api/v1/editorial/workflows/{id}/publish` | Validate and activate a machine version | `content.workflow.manage` |
| POST | `/api/v1/content/items/{id}/workflow/transition` | Apply a transition (body: transition id, comment, assignee) | `content.workflow.transition` |
| GET | `/api/v1/content/items/{id}/workflow` | Timeline, current state, allowed transitions, approval status | `content.workflow.read` |
| PUT | `/api/v1/content/items/{id}/workflow/assignee` | Set or clear the assignee | `content.workflow.assign` |
| GET · POST | `/api/v1/content/revisions/{id}/comments` | List · create a revision comment | `content.workflow.comment` |
| PATCH · DELETE | `/api/v1/content/comments/{id}` | Resolve/unresolve · delete (author or manager) | `content.workflow.comment` |
| GET | `/api/v1/editorial/my-work` | Assigned and awaiting-review queues for the caller | `content.workflow.read` |
| GET | `/api/v1/editorial/calendar` | Schedules and due dates for a range, with conflicts | `content.workflow.read` |
| POST · DELETE | `/api/v1/content/items/{id}/schedule` | Create publish/unpublish/delete/theme-change schedule · cancel | `content.pages.schedule` |
| POST · DELETE | `/api/v1/content/items/{id}/preview-links` | Create a preview link from a previewable state · revoke | `content.preview.manage` |
| GET | `/api/v1/preview/{token}` | Render the working revision for a token (no panel session) | — |

The public preview route short-circuits the panel auth stack by design, is rate-limited per IP, and returns `404` for an expired, revoked or unknown token with no distinguishing detail.

### Data model

Migration: `0117_editorial_workflow.sql` (next free number; append-only ledger — shift up if taken).

```sql
workflow_definitions (id uuid pk, organization_id uuid, site_id uuid, content_type text, name text,
  version int default 1, status text in ('draft','active','archived') default 'draft',
  created_by uuid -> users, created_at/updated_at)  unique (site_id, content_type) where status = 'active'
workflow_states (id uuid pk, definition_id uuid -> workflow_definitions on delete cascade, key text, label text, colour text,
  position int, category text in ('working','review','ready','live','closed'), is_initial bool default false,
  is_terminal bool default false, previewable bool default false, deprecated bool default false, created_at/updated_at)
  unique (definition_id, key)
workflow_transitions (id uuid pk, definition_id uuid on delete cascade, from_state_id uuid -> workflow_states,
  to_state_id uuid -> workflow_states, permission text not null, role_id uuid null -> roles,
  require_assignee bool default false, approval_gate bool default false, approver_role_id uuid null -> roles,
  due_days int null, require_comment bool default false, notify bool default true, created_at/updated_at)
  index (definition_id, from_state_id)
content_workflow_items (content_item_id uuid pk, definition_id uuid, state_id uuid -> workflow_states,
  assignee_user_id uuid null -> users, assignee_role_id uuid null -> roles, due_at timestamptz null,
  approval_request_id uuid null, entered_state_at timestamptz, created_at/updated_at)
  index (state_id, due_at), (assignee_user_id, due_at)
content_workflow_events (id uuid pk, content_item_id uuid, from_state_id uuid null, to_state_id uuid,
  actor_user_id uuid null, comment_id uuid null, created_at timestamptz)  index (content_item_id, created_at)
revision_comments (id uuid pk, revision_id uuid -> page_revisions on delete cascade, parent_id uuid -> revision_comments,
  author_user_id uuid, body text, resolved_at timestamptz null, resolved_by uuid null, created_at/updated_at)
  index (revision_id, created_at)
revision_comment_mentions (comment_id uuid -> revision_comments on delete cascade, mentioned_user_id uuid -> users,
  primary key (comment_id, mentioned_user_id))
editorial_schedule (id uuid pk, organization_id uuid, site_id uuid, content_item_id uuid null, kind text in
  ('publish','unpublish','delete','theme_change'), scheduled_at timestamptz, timezone text default 'UTC',
  payload jsonb default '{}' /* theme_change: from/to theme keys */, status text in ('pending','done','failed','cancelled')
  default 'pending', result text, error text, created_by uuid, claimed_at timestamptz null, created_at/updated_at)
  index (status, scheduled_at) where status = 'pending'  index (site_id, scheduled_at)
content_preview_links (id uuid pk, content_item_id uuid, token_hash text unique, created_by uuid,
  expires_at timestamptz, revoked_at timestamptz null, last_used_at timestamptz null, created_at)
  index (content_item_id) where revoked_at is null
```

Notes. One queue, four kinds: if the earlier scheduling table from the CMS pack ships first, its rows move into `editorial_schedule` and its screen becomes a filtered view — two queues must not diverge. The runner claims due rows atomically (`for update skip locked`), records a result per row, and never retries a `delete` twice by keying idempotency on the row id. Preview tokens are stored hashed and single-use-per-render is not required; revocation plus expiry is the contract. Workflow events are append-only and are the source of the timeline.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `content.workflow.transitioned` | Any state change, including publish into `live` | `item_id`, `type`, `from`, `to`, `actor_user_id` |
| `content.workflow.assigned` · `.unassigned` | Assignee set or cleared | `item_id`, `assignee_user_id`, `assignee_role_id`, `due_at` |
| `content.workflow.comment.created` · `.resolved` | Revision discussion | `comment_id`, `revision_id`, `mentions` |
| `content.workflow.approval.requested` · `.decided` | Gate opened or released | `item_id`, `transition_id`, `decision` |
| `content.schedule.created` · `.fired` · `.failed` · `.cancelled` | Schedule lifecycle, all four kinds | `schedule_id`, `kind`, `scheduled_at`, `result` |
| `content.preview_link.created` · `.revoked` | Preview access changed | `item_id`, `link_id`, `expires_at` |

Consumed: `approvals.request.decided` (releases or refuses a gated transition; the workflow never polls), `identity.user.deactivated` (reassigns open items to the role and flags them), `themes.activated` (closes a `theme_change` schedule with a result). Webhook relevance: `content.workflow.transitioned` and `content.schedule.fired` are the two events external tools need; payloads carry ids and state keys, never comment bodies or preview tokens.

### Acceptance criteria

- [ ] The default machine is seeded for every content type; a publish moves the item through Draft → In review → Approved → Published, and the timeline shows actor and timestamp per step.
- [ ] The machine editor refuses to save two initial states, no initial state, an unreachable state, or a `live` state with no inbound transition, each with a stated reason.
- [ ] A transition guarded by a permission is absent for a user without it and present for one with it; a guarded transition shows its reason when it is disabled.
- [ ] With `assignee required` on a transition, the action is refused until an assignee is chosen, and the chosen assignee sees the item under `Assigned to me`.
- [ ] With the approval gate on, the transition is refused until the approver decides; the decision arrives through the approval engine and the transition then completes without a second click by the requester.
- [ ] `require comment` refuses the transition with an empty comment and the comment lands on the current revision, visible in the item's comment pane.
- [ ] A mention produces a notification for the mentioned user and a mention row; resolving hides the thread behind a `Show resolved` toggle without deleting it.
- [ ] A schedule for each of the four kinds fires within a minute of its time in the site timezone, appears as `done` with a result, and a failure shows `failed` with an error and a retry action.
- [ ] A theme change schedule flips the site theme at the scheduled time, records the previous theme in its payload, and one click restores it.
- [ ] The calendar shows the schedules of the selected range, highlights a same-minute pair as a conflict, flags an item scheduled while it cannot reach `live`, and flags a schedule whose assignee is deactivated.
- [ ] Drag-rescheduling a calendar item writes the new time, keeps one row (no duplicate), and shows the new time in the site timezone in the confirmation.
- [ ] A preview link renders the working revision for a signed-out visitor, stops working after expiry, and is revoked automatically when the item reaches `live`.
- [ ] Transitioning to `live` publishes exactly the revision that was approved; a newer draft revision created after approval is not published and is flagged in the timeline.
- [ ] Deprecating a state that still holds items is refused until the items are moved out, and the move writes one workflow event per item.
- [ ] Archived content types with in-flight items cannot have their machine deleted; deleting an empty machine leaves items untouched and returns them to the default machine.
- [ ] All new screens render at 390 px without horizontal scroll; the calendar is usable on mobile as a list per day; the walkthrough reports zero high findings.

### QA plan

The walkthrough visits `/editorial/workflow` (edit the default machine, add a review state, publish the machine), then a page's Workflow tab (move Draft → In review with a comment and an assignee, approve under the gate, publish), comments (mention a user, resolve a thread), preview links (open one signed-out, revoke one, check the automatic revocation after publish), `/editorial/my-work` (see the assigned item and act on it), `/editorial/calendar` (schedule each of the four kinds, create a same-minute conflict, drag-reschedule one, cancel one), and `/editorial/settings` (change reminder lead time). Operations check: run a scheduled publish and a scheduled delete with a short clock window and confirm the results in the queue and in the events stream. Visual check: the machine editor shows real columns and arrows, the timeline shows real state cards, the calendar shows coloured items in a real grid, and conflict flags render as badges with counts.

### Slices

1. **Machine and transitions.** Migration `0117_editorial_workflow.sql` (definition, state, transition and item tables plus the seeded default machine); machine editor with validation, item Workflow tab with guarded transitions and the timeline, permissions wiring. *Done when:* acceptance 1–4, 13 and 15 pass and the page Workflow tab is in the walkthrough inventory.
2. **Schedule family and calendar.** `editorial_schedule` plus the atomic runner for the four kinds, schedule dialog and queue, calendar with conflict classes, drag reschedule, theme-change revert, queue reconciliation with the earlier scheduling table if it shipped. *Done when:* acceptance 8–11 pass and a short-clock publish plus delete fire in the QA environment.
3. **Review collaboration and preview access.** Revision comments with threads, mentions and resolution; assignments and `/editorial/my-work`; approval gate wiring; preview links with expiry and state-bound revocation; reminder events. *Done when:* acceptance 5–7, 12 and 14 pass, and the QA report lists zero high findings for the wave.

### Risks / notes

- The state machine is per type per site and must stay small: dozens of states make the calendar and the guards unreadable. The editor warns above eight states and blocks above twelve.
- Transition rules are evaluated server-side only. The UI hiding a button is a courtesy; every transition endpoint re-checks permission, role, assignee, gate and comment requirement in one place, or the panel becomes a suggestion engine.
- The scheduler is the highest-risk piece: claim rows atomically, record a result, and make `delete` idempotent. A restart must never delete twice or lose an entry; the reconciliation on boot scans for rows claimed but not resolved older than the claim window.
- A `theme_change` schedule changes what every visitor sees. It always stores the previous theme, is refused when the target theme is missing, and reverting must not queue a second change.
- Preview tokens are bearer credentials: hashed at rest, expiring, revocable per item, revoked on publish, and never logged. The preview route must not enumerate item titles on a bad token.
- Approval gating delegates to REQ-059 when present and falls back to the two-person rule; the fallback must be visibly labelled in the UI so nobody believes a formal chain is in force when it is not.
- Comment bodies are user content: sanitised on render, mention parsing never executes markup, and the notification fan-out is capped per item per hour to stop a thread from becoming a mail bomb.
- The publisher must publish the approved revision only. Storing the approved revision id on the workflow item at approval time is the cheapest correct fix; recomputing "latest draft" at publish time is the bug this REQ exists to prevent.
