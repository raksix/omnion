> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** module (`modules/calendar`)
> **Source:** owner brief — business suite / frontend depth (docs/08-BUSINESS-SUITE.md, docs/03-FRONTEND.md)

## Request

Time and appointments.

- **Calendar views**: day/week/month/agenda, multiple calendars (personal, team, resource), drag to move/resize.
- **Events**: title, attendees (internal users + external e-mail), location, description, reminders, repeat rules (basic RRULE).
- **Appointment booking**: public booking page per staff/resource with availability windows, buffers, blackout dates, iCal export.
- **Reminders**: notification + e-mail via notification center; reminder rules per calendar.
- **Integration**: leave/absence overlay (HR), task due overlay (Projects).
- **Events**: `calendar.event.created`, `calendar.booking.confirmed`.

## Implementation spec

> **Module:** `modules/calendar` (crate `omnion-module-calendar`, workspace member) · **Migration:** `database/migrations/0017_calendar.sql` (next free slot at build time) · **Admin routes:** `/calendar/*` · **Public route:** `/book/{slug}` on the site renderer · **Permission family:** `calendar.*` · **Depends on:** core crates (`identity`, `notifications`, `events`) + `modules/hr` (REQ-055) and `modules/projects` (REQ-056) for overlays.

### Scope (in / out)

**In**

- Calendars of three kinds: personal (per user), team (shared, members with viewer/editor roles) and resource (room, equipment, staff post); each with time zone, colour and default reminder rules.
- Events: title, description, location, start/end (with time zone), all-day flag, attendees (internal users and external e-mail addresses) with response state, visibility (`busy` / `free` / `private`), reminders, and a basic repeat rule (RRULE subset: `DAILY`/`WEEKLY`/`MONTHLY`, `INTERVAL`, `BYDAY`, `COUNT`, `UNTIL`).
- Views: day, week, month, agenda; multiple calendars overlaid with toggles; drag to move and resize (plus a full keyboard path: nudge by 15 min / 1 day, change duration with `alt + ↑/↓`).
- Recurrence: edit one occurrence, this-and-following, or the whole series; cancelled occurrences stay visible as struck-through exceptions.
- Booking: public booking pages per staff member or resource with duration options, buffers, minimum notice, daily cap, availability windows, blackout dates, and a tokenized manage/cancel link; iCal export for calendars and single events.
- Reminders: per-calendar rules (offset + channel `in_app` / `email`) applied to new events with a per-event override; delivered through REQ-021.
- Overlays: HR leave/absence (`hr.leave.approved`, REQ-055) and task due dates (`projects.task.due_soon`, REQ-056) rendered as read-only layers with legends; a task due overlay row opens the task drawer on the projects screen.
- Conflict awareness: creating or moving an event onto a busy attendee or resource shows a non-blocking conflict warning listing the clashing bookings (and a hard block when the resource policy says so).

**Out (tracked elsewhere)**

- Video-conference link provisioning, calendar sync (Google/Microsoft two-way), SMS reminders → future integration requests (REQ-015). Room booking approvals → REQ-059. Timezone-aware payroll impact → out of scope.
- The notification transport and e-mail templates → REQ-021. iCal import (only export here); CalDAV → later. Recurrence outside the listed RRULE subset is refused with a clear message rather than silently ignored.

### Screens (UI)

Module nav: **Calendar · Bookings · Booking pages · Availability · Reminders · Settings**.

| Route | Screen |
|---|---|
| `/calendar` | Default view (week) with the calendar list rail, mini month, today button, view switcher |
| `/calendar/day`, `/calendar/week`, `/calendar/month`, `/calendar/agenda` | The four views (state in the URL, deep-linkable with `?date=&calendars=`) |
| `/calendar/events/new`, `/calendar/events/{id}` | Event form / event detail (drawer over the current view, deep-linkable) |
| `/calendar/calendars` | Calendar manager: create/edit calendars, members and roles, colours, default reminders |
| `/calendar/availability` | Availability editor: weekly windows per user/resource, time zone, blackout dates |
| `/calendar/bookings` | Bookings list with status tabs (Upcoming / Past / Cancelled / No-show) |
| `/calendar/booking-pages`, `/calendar/booking-pages/{id}` | Page list / editor (slug, owner, durations, buffers, notice, cap, texts, active) |
| `/calendar/reminders` | Reminder rules per calendar + delivery log of the last 50 reminders |
| `/calendar/settings` | Week start, default duration, default view, time-zone display mode, overlay toggles |

**Views** — week grid: time axis (00–24, configurable working-hours zoom), 7 day columns, all-day row on top, now-line; events are blocks with title, time range and attendee avatars, truncated with a `+2` chip and a tooltip. Month: classic grid with up to 3 chips per day and a `+N more` popover. Agenda: chronological list grouped by day (Overdue/today/tomorrow/next 7 days/later) with the same fields as the detail. Drag to move (snap 15 min) and drag the bottom edge to resize; keyboard: focus a block, `←/→` move 15 min (with `shift` one day), `alt + ↑/↓` resize, `enter` open, `del` delete with confirm. Every view supports keyboard navigation from the toolbar (`d/w/m/a` switches view, `t` today, `n` new event, `/` jump-to-date, `?` shortcuts).

**Calendar rail** — checkbox per visible calendar (colour swatch + name + count), grouped Personal / Team / Resources, overlay toggles for "Absences (HR)" and "Task due dates (Projects)" with a legend; the selection is remembered per user. A calendar the caller cannot read is not listed.

**Event form** — Title (required ≤200), Calendar (required), All day (toggle), Starts (date + time, required), Ends (required, > starts; changing starts shifts ends keeping the duration unless edited), Time zone (default: the calendar's, with an explicit selector so a cross-zone booking is visible as such), Repeat (select: none / daily / weekly / monthly + interval, weekdays, until or count; an unsupported pattern is refused with a reason), Location (free text + optional resource picker), Attendees (internal combobox + external e-mail chips, validated `[^@\s]+@[^@\s]+\.[^@\s]+`, duplicates collapsed), Visibility (`busy` / `free` / `private`; private hides the title from non-attendees and shows "Private" in the grid), Description (markdown-lite ≤4000), Reminders (list of offsets with channel, defaulted from the calendar, editable per event). Validation is field-level; the conflict warning appears under the time fields with the conflicting items listed and a "keep anyway" confirmation when the policy allows it.

**Event detail** — same fields read-only, plus attendee response states (invited/accepted/tentative/declined), an RSVP control for the caller, the per-occurrence actions for a series (this / following / all), and an activity log (created, moved, invite sent, responses). Delete asks the scope for a series event.

**Bookings** — list columns: `When`, `Booking page` (staff/resource), `Name`, `E-mail`, `Phone`, `Status` (badge), `Notes`, `Source` (`public page` / `manual`), `Created`. Filters: status, page/owner, date range, "has note". Row actions: open (drawer with the full booking + the linked calendar event), reschedule (drag in a week view side panel), cancel (with reason, sends the cancellation e-mail), mark no-show, copy the manage link (never the raw token in logs). Manual booking creation from the panel is allowed with `calendar.bookings.manage`.

**Booking pages** — editor: slug (required, unique, `^[a-z0-9-]{2,48}$`, live preview of `/book/{slug}`), title, owner kind (`staff` → one user, `resource` → one resource) and target, duration options (1–8 entries, 15/30/45/60 minutes defaults), buffer before/after (0–120 min), minimum notice (0–10080 min), daily cap (1–50), booking horizon (days), welcome text, confirmation text, cancellation policy text, active toggle, iCal invite toggle. The availability tab links to `/calendar/availability` for that owner and shows a "next 7 available slots" preview computed live.

**Public booking page** (`/book/{slug}`, no sign-in) — server-rendered on the site renderer: title, welcome text, owner name, duration selector, month calendar with available days and a slot list (computed from availability minus existing events minus buffers minus notice/cap rules), a short form (Name required, E-mail required + validated, Phone optional, Note optional), then a confirmation screen with the manage/cancel token link and `Add to calendar` (`.ics` download). Disabled/unavailable states are explicit ("no slots in the next 14 days"), the page is `noindex` and rate-limited by IP, and a honeypot field plus a submit delay guard the form (no CAPTCHA dependency).

**Reminders** — rules table per calendar (offset, channel, applies-to: new events / all-day only), per-event override list, and a delivery log (event, offset, channel, sent-at, result) so a "reminder never arrived" question is answerable on screen.

**Mobile:** week view becomes 3 days with a horizontal swipe, month view a day list; event form is a full screen with native-ish date/time inputs and the time-zone field collapsed to a sum-up row; the public booking page is a single column with big slot buttons.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET/POST | `/api/v1/calendar/calendars` | Calendar list / create | `calendar.calendars.read` / `.manage` |
| PATCH/DELETE | `/api/v1/calendar/calendars/{id}` | Update / archive a calendar | `calendar.calendars.manage` |
| GET/POST/DELETE | `/api/v1/calendar/calendars/{id}/members` | Members + roles | `calendar.calendars.manage` |
| GET | `/api/v1/calendar/events` | Occurrences in a window (`?from=&to=&calendars=&overlays=`) | `calendar.events.read` |
| POST | `/api/v1/calendar/events` | Create (single or series) | `calendar.events.create` |
| GET/PATCH | `/api/v1/calendar/events/{id}` | Detail (occurrence-aware `?occurrence=`) / update | `calendar.events.read` / `.update` |
| POST | `/api/v1/calendar/events/{id}/move` | Move/resize (start, end) with conflict check | `calendar.events.update` |
| POST | `/api/v1/calendar/events/{id}/respond` | RSVP for the caller | `calendar.events.read` |
| POST | `/api/v1/calendar/events/{id}/cancel` | Cancel one occurrence / following / series | `calendar.events.update` |
| GET | `/api/v1/calendar/events/{id}/ics` | Single-event `.ics` | `calendar.events.read` |
| GET | `/api/v1/calendar/feed.ics` | Calendar feed by token (`?token=`) | token-scoped, read-only |
| GET/PUT | `/api/v1/calendar/availability` | Read / replace weekly windows (owner-scoped) | `calendar.availability.read` / `.manage` |
| GET/POST/DELETE | `/api/v1/calendar/blackouts` | Blackout dates | `calendar.availability.manage` |
| GET/POST | `/api/v1/calendar/booking-pages` | Pages list / create | `calendar.bookings.read` / `.manage` |
| PATCH/DELETE | `/api/v1/calendar/booking-pages/{id}` | Update / deactivate | `calendar.bookings.manage` |
| GET | `/api/v1/calendar/slots` | Available slots for a page in a window | `calendar.bookings.read` |
| GET | `/api/v1/calendar/bookings` | Booking list (filters, tabs) | `calendar.bookings.read` |
| POST | `/api/v1/calendar/bookings/{id}/cancel` | Cancel + notify | `calendar.bookings.manage` |
| POST | `/api/v1/calendar/bookings/{id}/reschedule` | Move to another slot | `calendar.bookings.manage` |
| GET/POST | `/api/v1/calendar/public/pages/{slug}` | Public page payload (no session) | — (public) |
| POST | `/api/v1/calendar/public/pages/{slug}/book` | Public booking (rate-limited, honeypot) | — (public) |
| POST | `/api/v1/calendar/public/bookings/{token}/cancel` | Public cancel via token | — (public, token-scoped) |
| GET/POST | `/api/v1/calendar/reminder-rules` | Reminder rules per calendar | `calendar.calendars.manage` |

Occurrence expansion caps at 500 occurrences per request and answers a clear error beyond that; client-supplied time zones are validated against the platform's time-zone table.

### Data model

```text
calendar_calendars(id uuid pk, organization_id uuid not null, name text not null,
  kind text not null default 'personal', owner_user_id uuid, timezone text not null default 'UTC',
  color text not null default '#3b82f6', default_reminder_minutes integer[] not null default '{30}',
  is_default boolean not null default false, archived_at timestamptz, created_at timestamptz, updated_at timestamptz)
calendar_members(id uuid pk, calendar_id uuid not null references calendar_calendars(id) on delete cascade,
  user_id uuid not null, member_role text not null default 'viewer')
calendar_events(id uuid pk, organization_id uuid not null, calendar_id uuid not null, title text not null,
  description text not null default '', location text, starts_at timestamptz not null, ends_at timestamptz not null,
  timezone text not null, all_day boolean not null default false, visibility text not null default 'busy',
  rrule text, rrule_until timestamptz, rrule_count integer, series_id uuid, original_starts_at timestamptz,
  event_status text not null default 'confirmed', resource_id uuid, created_by uuid,
  cancelled_at timestamptz, created_at timestamptz, updated_at timestamptz)
calendar_attendees(id uuid pk, event_id uuid not null references calendar_events(id) on delete cascade,
  user_id uuid, email citext, attendee_kind text not null, response text not null default 'invited',
  responded_at timestamptz, response_note text)
calendar_event_reminders(id uuid pk, event_id uuid not null references calendar_events(id) on delete cascade,
  offset_minutes integer not null, channel text not null default 'in_app', sent_at timestamptz)
calendar_resources(id uuid pk, organization_id uuid not null, name text not null, kind text not null,
  location text, capacity integer, owner_user_id uuid, active boolean not null default true)
calendar_availability(id uuid pk, organization_id uuid not null, owner_kind text not null, owner_user_id uuid,
  resource_id uuid, weekday smallint not null, start_time time not null, end_time time not null, timezone text not null)
calendar_blackouts(id uuid pk, organization_id uuid not null, owner_kind text not null, owner_user_id uuid,
  resource_id uuid, starts_at timestamptz not null, ends_at timestamptz not null, reason text not null default '')
calendar_booking_pages(id uuid pk, organization_id uuid not null, slug text not null, title text not null,
  owner_kind text not null, owner_user_id uuid, resource_id uuid, durations integer[] not null default '{30}',
  buffer_before_minutes integer not null default 0, buffer_after_minutes integer not null default 0,
  min_notice_minutes integer not null default 120, daily_cap integer not null default 8, horizon_days integer not null default 30,
  welcome_text text not null default '', confirmation_text text not null default '', policy_text text not null default '',
  active boolean not null default true)
calendar_bookings(id uuid pk, organization_id uuid not null, page_id uuid not null, calendar_event_id uuid,
  starts_at timestamptz not null, ends_at timestamptz not null, timezone text not null, name text not null,
  email citext not null, phone text, note text not null default '', booking_status text not null default 'booked',
  token_hash text not null, source text not null default 'public', created_at timestamptz, cancelled_at timestamptz,
  cancel_reason text)
calendar_reminder_rules(id uuid pk, organization_id uuid not null, calendar_id uuid not null,
  offset_minutes integer not null, channel text not null default 'in_app', applies_to text not null default 'all')
```

Checks: `kind in ('personal','team','resource')`; `visibility in ('busy','free','private')`; `event_status in ('confirmed','tentative','cancelled')`; `attendee_kind in ('internal','external')`; `response in ('invited','accepted','tentative','declined')`; `booking_status in ('booked','cancelled','completed','no_show')`; `ends_at > starts_at`; `all_day` events must span whole days in their time zone; `weekday between 0 and 6`; `start_time < end_time`; `min_notice_minutes >= 0`; `daily_cap between 1 and 50`; `durations` elements between 5 and 480; `rrule` must match `^(FREQ=(DAILY|WEEKLY|MONTHLY))(;INTERVAL=[0-9]+)?(;BYDAY=[A-Z,]+)?(;COUNT=[0-9]+)?(;UNTIL=[0-9TZ:.-]+)?$` and may not carry both `COUNT` and `UNTIL`; unique `(organization_id, lower(slug))` on pages, unique `(page_id, starts_at)` for `booked` rows, unique `(calendar_id, series_id, original_starts_at)` for exception rows.

Indexes: `calendar_events_window_idx (calendar_id, starts_at, ends_at)`, `calendar_events_org_starts_idx (organization_id, starts_at)` plus a GiST-free range check in the service, `calendar_events_series_idx (series_id)`, `calendar_attendees_user_idx (user_id)`, `calendar_bookings_page_start_idx (page_id, starts_at)`, `calendar_bookings_org_status_idx (organization_id, booking_status, starts_at)`, `calendar_availability_owner_idx (owner_kind, owner_user_id, resource_id, weekday)`, `calendar_event_reminders_due_idx (offset_minutes) where sent_at is null`.

Recurrence model: one row per series (`rrule` set) plus one row per exception (moved/cancelled occurrence, `series_id` + `original_starts_at`); expansions are computed on read, never stored in bulk. Booking tokens are stored hashed (SHA-256) exactly like the sales public link. Migration: `database/migrations/0017_calendar.sql`, additive; seeds a personal calendar per existing user (and on user creation) plus one team calendar named "Company" and availability windows Mon–Fri 09:00–18:00 in the organization's time zone for the organization owner.

### Events

Emitted: `calendar.event.created`, `calendar.event.updated`, `calendar.event.cancelled`, `calendar.event.rsvp`, `calendar.booking.confirmed`, `calendar.booking.cancelled`, `calendar.booking.no_show`, `calendar.booking_page.published`, `calendar.reminder.sent`. Payloads carry ids, start/end in UTC plus the event time zone, attendee user ids and (for bookings) the page slug and the booking name — never the token, never the full attendee e-mail list beyond the organization's own data. Consumed: `hr.leave.approved` (REQ-055) feeds the absence overlay, `projects.task.due_soon`/`projects.task.overdue` (REQ-056) feed the task overlay, `sales.quote.sent`/`crm.deal.*` (REQ-051/052) can create a follow-up event by rule, `user.created` (REQ-006) creates the personal calendar.

Webhook relevance: `calendar.booking.confirmed` and `calendar.booking.cancelled` are the names an external scheduling/CRM system subscribes to; `calendar.event.created` drives the documented automation (on a new external booking → create a CRM contact + an activity + a reminder task).

### Acceptance criteria

- [ ] Migration `0017_calendar.sql` applies on a populated database; `cargo test -p omnion-module-calendar` is green and creates a personal calendar per user.
- [ ] Every `/api/v1/calendar/*` route is permission-guarded; another organization's event or booking id answers 404; a viewer-only member cannot write.
- [ ] Event create/update/move/cancel, calendar changes, availability edits and booking decisions write audit entries.
- [ ] All four views render the same event set for a given window, with URL state (`?date=&calendars=`) surviving a reload and a shared link.
- [ ] Drag moves an event by 15-minute snaps and keyboard `←/→`/`alt + ↑/↓` produce identical results; both persist and are reversible.
- [ ] Resizing keeps `ends_at > starts_at` and refuses zero/negative durations with a field-level message.
- [ ] Recurrence: a weekly series with `BYDAY` expands correctly across a DST boundary in a named time zone (test fixture), and the occurrence cap is enforced with a clear error.
- [ ] Editing one occurrence creates an exception without touching the series; "this and following" splits the series; cancelling one occurrence shows it struck through in the view.
- [ ] Moving an event onto a busy attendee or resource shows the conflict list; a resource policy of "block" refuses the move with the conflicting item named.
- [ ] Invitations: internal attendees get an in-app notification and can RSVP (state visible on the event and in the grid); external attendees receive an e-mail with the `.ics`.
- [ ] A private event shows "Private" to non-attendees in every view while the owner sees the title; the API never leaks the title to non-attendees.
- [ ] `.ics` export of a calendar feed and a single event parses in a standard calendar client (verified by parsing the file in the test suite) and carries the correct time zone.
- [ ] Public booking page: slots respect availability windows, existing events, buffers, minimum notice, horizon and daily cap; a taken slot never appears as free (concurrency test: two simultaneous bookings for one slot produce one booking and one 409).
- [ ] Public booking creation sends a confirmation with the manage link; cancelling through the token frees the slot and emits `calendar.booking.cancelled`; an invalid/used token answers a friendly expired state, not a stack trace.
- [ ] Reminders fire once per offset on the right channel at the right time (a test clock proves it), appear in the delivery log, and per-event overrides beat the calendar defaults.
- [ ] HR absence and project due overlays render read-only with a legend, and toggling them off leaves the base calendars untouched.
- [ ] `calendar.event.created` and `calendar.booking.confirmed` appear in the event feed with the documented payload and reach a subscribed webhook.
- [ ] Empty, loading and error states exist on every screen; no dead buttons and no placeholder events.
- [ ] Mobile 390×844: day/week strip, agenda and the public booking page are usable one-handed.

### QA plan

Add to `scripts/qa/walkthrough.cjs`: `/calendar`, `/calendar/day`, `/calendar/month`, `/calendar/agenda`, `/calendar/calendars`, `/calendar/availability`, `/calendar/bookings`, `/calendar/booking-pages`, `/calendar/reminders`, `/calendar/settings` (desktop) plus `/calendar/agenda` and the booking page editor (mobile), and the public `/book/{slug}` page on the site host. The script must: create an event with an internal attendee and an external e-mail → drag it to another slot and nudge it back with the keyboard → create a weekly recurring event and edit one occurrence → toggle the HR and task overlays → activate a booking page → open the public page on the site host, pick a slot, submit the form, land on the confirmation → open the bookings list, reschedule and then cancel the booking → check the reminder log. It clicks every control on each screen (including view switchers, calendar checkboxes, the repeat editor and the public slot buttons).

Visual check: the week grid shows time axis, now-line and coloured blocks with attendee avatars and no clipped titles at 1440 px; overlays are visually distinct (hatched bars + legend naming them); the event drawer shows labelled fields with a visible conflict warning; the public page shows a month picker, a slot list and a real confirmation; the bookings list shows status badges. Screenshots: `page-calendar-week`, `page-calendar-month`, `page-calendar-bookings`, `web-booking-page`, `mobile-calendar-agenda`. Zero high findings; AA contrast on event text over calendar colours (test both light and dark); no token values visible in any screenshot.

### Slices

1. **Calendars + events + week/day views (data, API, screens).** Migration, calendar manager, event CRUD, drag/keyboard move, conflict warning, personal/team/resource kinds, audit, tests. Done when an event is created, dragged, nudged by keyboard and reloaded with the same result in all four views.
2. **Recurrence + attendees + reminders.** RRULE subset with exception model, invitations and RSVP, reminder rules with delivery log, notification integration. Done when a weekly series with one moved occurrence renders correctly and a reminder lands once in the log.
3. **Booking pages + availability + public flow.** Availability editor, blackouts, slot computation with buffers/notice/cap, public page with booking + confirmation + manage link, bookings list with reschedule/cancel/no-show, `.ics` exports. Done when the public walkthrough books a slot and the same slot cannot be double-booked.
4. **Overlays + reports-side polish + integration events.** HR absence and project task overlays, deep links into the source modules, settings screen, event emission verification and the automation example. Done when both overlays render with legends and `calendar.booking.confirmed` triggers an automation that creates a CRM activity.

### Risks / notes

- **Time is the hard part here:** store `timestamptz` plus the IANA time zone; expand recurrences in the owner's zone; test DST transitions (spring-forward and fall-back) explicitly. Any shortcut here produces "the meeting moved by an hour" bugs that never go away.
- **Double booking:** the booking insert must take a unique constraint on `(page_id, starts_at)` for booked rows plus a transactional slot re-check, otherwise a race produces two meetings in one room.
- **RRULE scope creep:** implement exactly the documented subset and refuse the rest with a readable message; a silent partial expansion is worse than an error.
- **Tokens and public pages:** hash the token, rate-limit by IP, `noindex`, no attendee list or internal description on the public payload, and never log tokens.
- **External e-mail invites** go through REQ-021; when it is not installed the page still books and shows "invite not sent" instead of pretending it was.
- **Overlays are read-only** and must be permission-filtered by the source module — a calendar block must never reveal an HR absence reason to someone who cannot read it.
- **Migration number** is the next free slot; renumber if a sibling module lands first.
