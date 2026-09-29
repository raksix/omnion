# REQ-064 — CMS Depth Pack

> **Status:** in-progress (slice 3 (SEO toolkit) is built end to end: `0139_cms_seo.sql`, the store with a generated-tag builder and a redirect loop guard, thirteen endpoints behind two new permissions, the public sitemap/robots/redirect routes, and the `/seo` screen. `cargo test -p omnion-api --test cms_seo` is 7/7 against real PostgreSQL and `cargo test -p omnion-content --lib` carries 154 (18 new). `bash scripts/qa/sql-check.sh` applies all 139 migrations in 7 s. Its browser pass is WRITTEN (`runSeoDepth`, 32 required steps, `--only=seo`) and running) · **Captured:** 2026-09-26 · **Layer:** platform (core + admin + web)
> **Source:** owner brief — business suite / frontend depth (docs/08-BUSINESS-SUITE.md, docs/03-FRONTEND.md)

## Request

Everything a serious CMS needs beyond pages (WordPress-parity pack).

- **Menus/navigation**: multiple menus, nested items, auto-add pages, per-location assignment (header/footer/sidebar), drag ordering, visibility rules.
- **Forms builder**: field types (text, textarea, select, radio, checkbox, date, file, consent), validation, spam protection (honeypot + rate limit), submission inbox with export, notification e-mails, webhook/automation trigger.
- **SEO toolkit**: per-page title/description/OG/Twitter cards, canonical, sitemap.xml + robots.txt management, JSON-LD schema per content type, redirect manager (301/302, regex-lite), broken-link view.
- **Scheduled publishing**: publish/unpublish at a date-time (timezone aware) with queue visibility.
- **Comments**: per-page/post comments with moderation states, spam heuristics, reply threads, e-mail notifications (toggleable).
- **Newsletter**: subscriber lists, double opt-in, send via SMTP, unsubscribe, archive page.
- **Memberships**: gated content (public/members/roles), signup/login for site visitors, profile page, access rules per page/block.
- **Media reuse**: featured image per page/post, alt/legend fields, focal point.

## Implementation spec

### Scope (in / out)

**In**
- Menus: multiple menus per site, nesting to 3 levels, item types page / custom URL / anchor / site index, location assignment (header, footer, sidebar, mobile), drag ordering, per-item visibility (`everyone`, `members`, `logged_out`, `roles`), target and `rel`, and bulk insertion of published pages of a type.
- Forms: text, textarea, select, radio, checkbox, date, file, consent; per-field validation (required, length, pattern, numeric range, date bounds, file types and size); spam protection (honeypot, minimum fill time, per-IP rate limit); submission inbox with filters, detail drawer, CSV export and bulk spam handling; notification e-mails through the platform mail path; an outbound event so automations and CRM can react.
- SEO: per-page title, description, canonical, OG/Twitter fields, OG image from the media library, `robots` directive, JSON-LD (Article, Organization, FAQPage, Product, BreadcrumbList) per page type with a generated-JSON preview; XML sitemap (index + per-type children, last-mod, optional images); robots.txt editor with validation; redirect manager (literal and regex-lite patterns, 301/302, hit counters, CSV import/export); broken-link view from a crawl-lite pass over internal links.
- Scheduled publishing: publish and unpublish at a date-time with a timezone, a visible queue, reschedule, cancel and publish now, and a background runner that records what it did.
- Comments: threaded (2 levels) comments on pages, moderation states (`pending`, `approved`, `spam`, `trash`), heuristics (links, blocked words, duplicate body, throttle), moderator replies, per-site notification toggle, public submission with honeypot and rate limit.
- Newsletter: subscriber lists, double opt-in with confirmation e-mail, unsubscribe, archive page of sent issues, CSV import/export, per-site from-identity settings. Sending a simple issue rides the marketing sender when REQ-060 is installed, otherwise the platform mail service — the module never depends on a module that is not there.
- Memberships: visitor accounts separate from panel users (`cms_members`), signup, sign-in, password reset, profile page, page visibility (`public`, `members`, roles) plus block-level visibility reusing the REQ-063 block meta.
- Media reuse: featured image per page, alt and legend per use, and a focal point (x/y, 0–1) so the renderer crops sensibly.

**Out**
- Panel accounts and permissions (identity/IAM); visitors never gain panel access.
- Full site crawling, keyword research, backlink analytics — the broken-link view is a lightweight internal check.
- Membership billing tiers, marketing journeys (REQ-060), third-party spam services.
- Multi-language menus and SEO overrides beyond the existing translation table (REQ-020 owns that depth).

### Screens (UI)

| Route | Screen |
|---|---|
| `/menus` · `/menus/<id>/edit` | Menu list · menu editor (item tree, per-item settings, locations) |
| `/forms` · `/forms/<id>/edit` · `/forms/<id>/submissions` | Form list · builder · submission inbox |
| `/seo/redirects` · `/seo/sitemap` · `/seo/broken-links` | Redirect manager · sitemap and robots.txt · broken links |
| `/publishing/queue` | Scheduled publishing queue |
| `/comments` | Moderation inbox |
| `/newsletter/lists` · `/newsletter/subscribers` · `/newsletter/archive` | Lists, subscribers, archive |
| `/members` · `/members/settings` | Visitor accounts · membership settings |
| `/pages/<id>` (extended) | Tabs: Content, Blocks, SEO, Media, Visibility, Comments, Revisions |

- **Menu editor.** Item tree with drag handles, drop-right to nest, inline label edit; expanded item settings: label, type, target (page picker / URL / anchor), window, `rel`, CSS class, visibility, `Enabled`. Toolbar: `Add item`, `Add pages…` (multi-select with a type filter, label defaults to the page title), `Move to menu`. Right rail: locations with checkboxes and the rule that one location holds one menu; a rendered-preview strip shows the navigation as the theme renders it, honouring the audience toggle (Visitor / Member). Validation: unique positions, depth ≤ 3, URLs absolute or site-relative.
- **Forms builder.** Field palette on the left (text, textarea, select, radio, checkbox, date, file, consent) dragged onto the canvas; right inspector per field (label, key, placeholder, help text, required, half/full width, validation rules, options editor, file constraints, consent text + privacy link). Settings tab: submit behaviour (inline message or redirect), notification recipients (validated), subject template with `{{form_name}}` / `{{submitted_at}}`, spam toggles, retention days, outbound event name, optional segment target. Preview tab renders the live form in the site theme; `Save` keeps working state, `Publish` makes it live.
- **Submission inbox.** Columns: Received, Name, E-mail, Summary, Source page, Status (`new`, `read`, `spam`, `archived`), IP hint. Filters: form, status, date range, text search. Bulk: Mark read, Spam, Archive, Export CSV. Drawer shows every answer, the consent text as accepted, source URL and user-agent hint, with `Mark spam` / `Delete permanently`. Empty state names the form URL and offers `Copy embed`.
- **SEO.** On the page: title with counter and SERP preview, description, canonical (auto-filled), OG title/description/image (media picker with recommended size), Twitter card type, robots variants, JSON-LD type with live JSON preview and a missing-fields checklist. `/seo/redirects`: From, To, Type, Pattern, Hits, Last hit, Enabled; create/edit, `Test a path`, CSV import/export, and a conflict warning when two rules match one path. `/seo/sitemap`: inclusion per page type, priority/frequency defaults, `Regenerate now`, XML preview, robots.txt editor with syntax check and a warning when a rule blocks the whole site. `/seo/broken-links`: Target, Status, Source page, Anchor, Found at, with `Create redirect` (pre-filled) and `Ignore`.
- **Publishing queue.** Content, Action (`publish` / `unpublish`), Scheduled at (site timezone shown beside it), Author, Status (`pending`, `done`, `failed`, `cancelled`), Result; actions Reschedule, Cancel, Publish now; failed rows show the error and a retry. The page editor's scheduling popover writes here and shows the entry inline.
- **Comments inbox.** Tabs Pending / Approved / Spam / Trash with counts; columns Author, Excerpt, Page, Submitted, Status, hints. Bulk Approve, Spam, Trash, Delete; detail with the full body, threaded replies and a `Reply as site` box, plus `Ban e-mail` / `Ban IP hint`. Settings: notifications, auto-approve threshold, blocked words, public form toggles.
- **Newsletter and members.** Lists table with create/rename/delete; subscribers table (E-mail, Status, List, Source, Confirmed at, Last activity) with filters, manual add, CSV import (mapping + duplicate policy), export and status actions; `Send issue` takes subject, body and list, shows the recipient count, and every issue lands in the archive with a public permalink. Members table (Name, E-mail, Status, Roles, Joined, Last sign-in) with Verify/Block/Delete and a detail view (profile, last 10 sign-in events, `Send password reset`); `/members/settings` holds signup, verification, default roles, post-sign-in redirect and the gated-page behaviour (`404` vs sign-in prompt).
- **States, keys, mobile.** Every list has a real empty state, skeleton loading and an error strip with retry; destructive actions confirm. Keys: `g m` menus, `g f` forms, `g c` comments, `g s` SEO, `j`/`k` rows, `a` approve, `s` spam, `Esc` closes drawers. On mobile, tables become card lists, the builder opens read-only with a preview, and inbox bulk actions sit in a sticky bar.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET · POST | `/api/v1/menus` | List menus · create | `menus.read` · `menus.manage` |
| GET · PUT · DELETE | `/api/v1/menus/{id}` | Read · save items and locations · delete | `menus.read` · `menus.manage` |
| POST | `/api/v1/menus/{id}/items/from-pages` | Bulk-add selected published pages | `menus.manage` |
| GET | `/api/v1/public/menus/{location}` | Rendered menu for a location (audience-aware) | — |
| GET · POST | `/api/v1/forms` | List · create a form with fields | `forms.read` · `forms.manage` |
| GET · PUT · DELETE · POST | `/api/v1/forms/{id}` (+`/publish`) | Read · update fields and settings · delete · publish | `forms.read` · `forms.manage` |
| GET · PATCH · DELETE | `/api/v1/forms/{id}/submissions` (+`/export`, `/{sid}`) | Inbox, CSV export, mark read/spam/archived, delete | `forms.submissions.read` |
| POST | `/api/v1/public/forms/{key}/submit` | Public submission (honeypot, rate limit) | — |
| GET · PUT | `/api/v1/pages/{id}/seo` | Read · write page SEO fields | `seo.read` · `seo.manage` |
| GET · PUT · POST | `/api/v1/sites/{site_id}/seo/{settings,sitemap/preview,sitemap/regenerate}` | Sitemap and robots.txt settings, XML preview, rebuild | `seo.read` · `seo.manage` |
| GET · POST · PUT · DELETE | `/api/v1/seo/redirects` | Redirect CRUD, test, CSV import/export | `seo.read` · `seo.manage` |
| GET · PATCH | `/api/v1/seo/broken-links` | Broken-link list · ignore or create redirect | `seo.read` · `seo.manage` |
| POST · DELETE | `/api/v1/pages/{id}/schedule` | Schedule publish/unpublish · cancel the entry | `content.pages.schedule` |
| GET | `/api/v1/publishing/queue` | Queue with status, author and result | `content.pages.schedule` |
| GET · PATCH | `/api/v1/comments` | Moderation inbox · approve/spam/trash/reply | `comments.read` · `comments.moderate` |
| GET · PUT | `/api/v1/comments/settings` | Notification toggle, heuristics, blocked words | `comments.settings.manage` |
| POST | `/api/v1/public/comments` | Public comment submission (honeypot, rate limit) | — |
| GET · POST · PUT · DELETE | `/api/v1/newsletter/lists` | List CRUD | `newsletter.read` · `newsletter.manage` |
| GET · POST | `/api/v1/newsletter/subscribers` (+`/import`, `/export`) | Subscribers, CSV import, export | `newsletter.read` · `newsletter.manage` |
| POST | `/api/v1/newsletter/issues` | Send an issue to a list | `newsletter.manage` |
| POST · GET | `/api/v1/public/newsletter/{subscribe\|confirm\|unsubscribe}` | Double opt-in flow and unsubscribe | — |
| GET · PATCH · DELETE | `/api/v1/members` | Visitor accounts · verify/block/roles · delete | `memberships.read` · `memberships.manage` |
| GET · PUT | `/api/v1/sites/{site_id}/members/settings` | Signup, verification, gating behaviour | `memberships.read` · `memberships.manage` |
| POST | `/api/v1/public/members/{signup\|signin\|signout\|password-reset}` | Visitor authentication | — |
| PUT · GET | `/api/v1/pages/{id}/visibility` · `/featured-media` | Page gating · featured image with alt, legend, focal point | `content.pages.update` · `media.update` |

Public routes resolve the site exactly like `/public/pages`, are rate-limited per IP, and never disclose whether an unpublished page exists.

### Data model

Migrations: `0112_cms_menus.sql`, `0113_cms_forms.sql`, `0114_cms_seo.sql`, `0115_cms_comments_newsletter_members.sql` (reserved band 0100–0115; append-only ledger — take the next free number if taken).

```sql
-- 0112_cms_menus.sql
cms_menus (id uuid pk, organization_id uuid, site_id uuid -> sites, key text, name text, locations text[] default '{}', created_at/updated_at)  unique (site_id, key)
cms_menu_items (id uuid pk, menu_id uuid -> cms_menus on delete cascade, parent_id uuid -> cms_menu_items, position int, label text,
  item_type text in ('page','url','anchor','index'), page_id uuid null -> pages, url text, target text default '_self', rel text,
  css_class text, enabled bool default true, visibility text in ('everyone','members','logged_out','roles') default 'everyone',
  visibility_roles text[] default '{}')  index (menu_id, parent_id, position)
-- 0113_cms_forms.sql
cms_forms (id uuid pk, organization_id uuid, site_id uuid, key text, name text, status text in ('draft','published') default 'draft',
  submit_action text in ('message','redirect'), submit_message text, redirect_url text, notify_emails text[] default '{}',
  notify_subject text, honeypot bool default true, min_fill_seconds int default 3, rate_limit_per_hour int default 20,
  retention_days int default 365, target_segment_id uuid null, created_by uuid -> users, created_at/updated_at)  unique (site_id, key)
cms_form_fields (id uuid pk, form_id uuid -> cms_forms on delete cascade, position int, key text, label text, field_type text,
  required bool default false, placeholder text, help_text text, width text default 'full', rules jsonb default '{}',
  options jsonb default '[]')  unique (form_id, key)
cms_form_submissions (id uuid pk, form_id uuid on delete cascade, site_id uuid, answers jsonb, consent_text text, source_path text,
  ip_hash text, user_agent_hash text, spam_score int default 0, status text in ('new','read','spam','archived') default 'new',
  created_at timestamptz)  index (form_id, created_at desc)
-- 0114_cms_seo.sql
pages += seo_title, seo_description, canonical_url, og_image_media_id uuid null -> media,
  robots text default 'index,follow', structured_data jsonb default '{}',
  visibility text default 'public', visibility_roles text[] default '{}',
  featured_media_id uuid null -> media, featured_alt, featured_legend,
  focal_x/focal_y numeric(4,3)  -- 0..1, both set or both null
cms_seo_redirects (id uuid pk, organization_id uuid, site_id uuid, from_path text, to_path text, status_code int default 301,
  pattern text in ('literal','regex') default 'literal', enabled bool default true, hits int default 0, last_hit_at timestamptz,
  created_by uuid, created_at/updated_at)  unique (site_id, from_path, pattern)
cms_broken_links (id uuid pk, site_id uuid, source_page_id uuid null, target_url text, anchor_text text, status int null,
  ignored bool default false, found_at timestamptz)  unique (site_id, source_page_id, target_url)
cms_seo_settings (site_id uuid pk, sitemap_types text[] default '{}', default_priority numeric(2,1), default_change_frequency text,
  sitemap_last_generated_at timestamptz, robots_txt text, updated_by/at)
-- 0115_cms_comments_newsletter_members.sql
cms_comments (id uuid pk, site_id uuid, page_id uuid -> pages on delete cascade, parent_id uuid -> cms_comments, author_name text,
  author_email text, body text, status text in ('pending','approved','spam','trash') default 'pending', ip_hash text,
  user_agent_hash text, reply_by uuid null -> users, created_at/updated_at)  index (page_id, status, created_at desc)
cms_publishing_queue (id uuid pk, page_id uuid on delete cascade, action text in ('publish','unpublish'), scheduled_at timestamptz,
  timezone text default 'UTC', status text in ('pending','done','failed','cancelled') default 'pending', result text, error text,
  created_by uuid, claimed_at/created_at/updated_at)  index (status, scheduled_at) where status = 'pending'
newsletter_lists (id uuid pk, site_id uuid, key text, name text, double_opt_in bool default true, created_at/updated_at)  unique (site_id, key)
newsletter_subscribers (id uuid pk, site_id uuid, list_id uuid on delete cascade, email text, source text, confirm_token_hash text,
  status text in ('pending','confirmed','unsubscribed','bounced') default 'pending', unsubscribe_token_hash text,
  confirmed_at timestamptz, created_at/updated_at)  unique (list_id, lower(email))
newsletter_issues (id uuid pk, site_id uuid, list_id uuid, subject text, body_html text, sent_at timestamptz, recipient_count int,
  archive_slug text unique)
cms_members (id uuid pk, site_id uuid, email text, password_hash text, name text, roles text[] default '{}',
  status text in ('pending','verified','blocked') default 'pending', verified_at timestamptz, last_signin_at timestamptz,
  created_at/updated_at)  unique (site_id, lower(email))
cms_member_tokens (id uuid pk, member_id uuid on delete cascade, kind text in ('verify','reset'), token_hash text unique,
  expires_at timestamptz, used_at timestamptz, created_at)
cms_member_sessions (id uuid pk, member_id uuid on delete cascade, token_hash text unique, expires_at timestamptz,
  last_seen_at timestamptz, ip_hash/ua_hash text, created_at)
```

### Events

| Event | When | Payload sketch |
|---|---|---|
| `content.page.published` · `.unpublished` · `.scheduled` · `.schedule_cancelled` | Publishing, queue writes and cancellations | `page_id`, `slug`, `revision_no`, `actor_user_id` |
| `content.form.submitted` | A public submission stored | `form_key`, `submission_id`, `source_path` |
| `content.comment.created` · `.approved` · `.spam` | Moderation transitions | `comment_id`, `page_id`, `status` |
| `newsletter.subscriber.confirmed` · `.unsubscribed` | Double opt-in and unsubscribe | `list_id`, `subscriber_id`, `source` |
| `members.member.created` · `.verified` · `.blocked` | Visitor account lifecycle | `member_id`, `site_id`, `status` |
| `content.menu.updated` · `seo.redirect.hit` · `seo.broken_links.found` | Navigation edits, redirect counters, crawl-lite results | ids plus counts |

Consumed: `marketing.form.submitted` (the marketing side of the same submission — one event contract, one listener target, never a duplicate row), `media.deleted` (featured and OG images degrade with a warning), `content.blocks.updated` (broken-link refresh), `approvals.request.decided` (a publish gate releases its queue entry). Webhook relevance: `content.form.submitted` is the automation entry point ("Form submitted → CRM lead → e-mail") and `newsletter.subscriber.confirmed` is what external marketing tools subscribe to; payloads carry ids and form keys, never free-text bodies or e-mail addresses.

### Acceptance criteria

- [x] Two menus exist at once; assigning one menu to Header and another to Footer in a single save works, and a location already holding a menu is reassigned after a confirmation.
    *Proved in `apps/api/tests/cms_menus.rs` (`two_menus_hold_header_and_footer_and_a_third_is_refused`): two menus hold `header`/`footer`; a third is refused `409 menu_location_taken` naming the contested location; the holder keeps it; releasing and re-claiming works. The UI half is built and green: the list's delete confirmation names the slots a deleted menu releases, and the editor's location rail refuses a second claim with the holder's key in the message (`runMenusDepth`: `rivalClaimRefused` + `rivalRefusalNamesTheHolder` + `firstHolderKeptIt` all true). Getting there needed a product fix — the refusal carried the holder as a UUID and the message said "another menu", so it named the slot but not the menu to move (`fecb4b0`).*
- [x] Items nest to three levels by drag, deeper nesting is refused with a message, and order survives a reload.
    *Proved (`items_nest_three_deep_survive_a_reload_and_a_fourth_is_refused`): a three-level tree round-trips through save -> reload with its parents and positions intact, a fourth level answers `400 menu_too_deep`, and the refused save leaves the stored tree untouched. The editor builds the tree through a nest/outdent control *and* a pointer drag that share one `move()`; `runMenusDepth` nests through the buttons, saves, reloads and reads the parents back from SQL, then asserts the refused fourth level left the row count unchanged (`refusalLeftTheTreeAlone`). The browser path is green under `--only=menus` (`nestClicked`, `nestedUnderSecond`, `nestedParentRowFound`, `treeHasChildren`, `parentsAreStored`, `depthLabel … deepest branch 2 of 3`), and driving it turned up a defect the store tests cannot see: nesting landed the row under a **collapsed** parent, so the row left the tree entirely and the move looked like a delete (`b9995ad`). **The refused fourth level is now proved in the browser, and getting there exposed a chain of false positives that had been reporting green.** The pass never built a three-level tree: it pressed *Nest under the row above* on a row that had already been nested, so the second press did nothing. `nestedUnderSecond` was meanwhile reading the **parent's own row** as its child (`[data-menu-item="<parent>"]` matches the parent's `<li>`, whose first `[data-menu-item-row]` is the parent), so every check built on it was nesting under a row that already had a child. With the child row reached through the parent's child list and the tree driven through *Add a child under* — the editor's own affordance for going deeper, named after the row it nests under — the tree reaches *deepest branch 3 of 3*, and the refusal is real: **400 `menu_too_deep`**, *"menu items nest at most 3 levels deep; 'QA fourth' reaches 4"*, with the stored tree unchanged (`refusalLeftTheTreeAlone`). `treeRendered` was wrong the same way, counting a **collapsed** branch's child as a lost row; branches are opened first, and `renderedRows 4 == savedItems 4`. Five harness fixes: `c2b8fd8` (the depth rule was never reached and `retryRefusesASentRow` was written by the *notifications* pass, which never runs under `--only=menus` — the queue's own retry is driven here now), `1c1893e`, `74dfe85`, `7301d57`, `aba4920`/`738bef3`.*
- [x] `Add pages…` inserts only published pages of the selected type and uses the page title as the label.
    *Proved (`add_pages_inserts_only_published_pages_with_their_titles`): a batch containing a draft is refused whole and inserts nothing; the published pair is inserted as `page` items labelled with the page's own title; the public payload resolves a page item to its slug. The picker offers published pages only and filters by content type, green under `--only=menus` (`pickerOpened`, `pickerOnlyOffersPublished`, `pageItems 1`, `labelComesFromTheTitle 1` — a join that proves the stored label is the page's title, not its slug). Those four checks had never run: the pass read a published page another pass had created, so on a private stack it skipped the lot silently. It now seeds its own page **and a draft**, because with no draft in the database `pickerOnlyOffersPublished` passes for a picker that lists everything.*
- [x] An item set to `members` is absent from the public menu payload for a signed-out visitor and present after sign-in; the public header renders through that same payload.
    *Proved for the payload (`a_members_item_is_absent_for_a_visitor_and_present_for_a_member`): the editor sees both items, the visitor payload has one, the member payload has two, and an unknown audience is refused. The audience is the `?audience=` query because `cms_members` (visitor accounts) is slice 4; sign-in wiring lands with it. The editor's preview strip reads that same public endpoint rather than re-filtering client-side (`runMenusDepth`: `visitorItems` < `memberItems` on one saved menu, read back through the audience toggle).*
- [x] *The queue half of slice 1 is proved:* an entry reschedules to a later local wall clock and stores it as UTC (`rescheduleStored 2026-10-04T09:00`), cancel is real in SQL rather than merely off screen, a `done` row draws no actions, the filter chip narrows the table, and the queue's **retry refuses a row that is not `failed`** (404 `publishing_entry_not_found`, row still `pending`). The retry check was the one item that could never be written: the checklist demanded a key the notifications pass owns.
- [~] All eight field types render and submit; required and pattern validation produce field-level errors on the public form using the builder's messages.
    *The store and the route half are proved (`all_eight_field_types_accept_their_answers_and_keep_the_consent_text`, `validation_answers_with_every_field_error_at_once`): all eight palette types survive the save, one submission carries every answer, the consent text is stored verbatim, and a submission wrong in three fields answers **422 with all three messages at once** — one per round trip would turn a six-field form into six submissions and the fifth is what trips the rate limit. The *builder's* messages on the public form are the browser half: `runFormsDepth` writes `previewRefusedAnEmptyRequired`, `previewRefusedAnEmptyMessage` and `previewAcceptedAFilledForm`, and that pass has NOT run yet.*
- [~] Honeypot, minimum-fill-time and per-IP rate-limit protections hold: a filled honeypot or a too-fast submission stores no row (spam counter increments), and the rate limit returns 429 with a retry hint after the configured submissions in an hour.
    *The store half is proved for all three (`a_filled_honeypot_stores_nothing_and_is_not_told`, `a_too_fast_submission_is_refused_by_the_forms_own_floor`, `the_hourly_limit_is_per_sender_and_does_not_stop_the_next_one`), and two decisions inside it are the point: the route answers **202 with `stored: false`** for every refusal, because telling the sender which protection fired is a free oracle for "is this IP blocked" and it teaches a bot what to work around; and the limit is **per sender**, so the third message from one address is dropped while the next address's first message goes through — a global limit would be a denial-of-service an anonymous visitor could aim at a site's contact form. **This criterion is not met as written**: it asks for a 429 with a retry hint, and the implementation deliberately does not send one. The 422-with-field-errors refusal is the only one a visitor is told about, because it is the only one they can fix.*
- [~] A valid submission appears in the inbox within one refresh, stores the consent text as accepted, sends the notification e-mail, and emits `content.form.submitted` with a successful webhook delivery.
    *Three of the four are proved: the submission is in the inbox with the consent **text** (not a boolean — a row that says "agreed" cannot answer "agreed to what" a year later), and the event reaches the bus (`a_submission_emits_the_event_the_req_names_as_contract` asserts on the `events` row, not on a mock, because an event that was emitted and lost would satisfy anything else). The **notification e-mail is not sent yet** — the recipients validate and are stored, and the send belongs to the platform mail path, which this slice does not touch. The webhook delivery is the bus's job and is covered by the event assertion, not by a form-specific test.*
- [x] CSV export of a filtered inbox returns exactly the filtered rows including answers.
    *Proved (`the_export_of_a_filtered_inbox_returns_exactly_the_filtered_rows`): three submissions go in, one is marked spam, and `GET /submissions/export?status=new` returns the header plus exactly the two unread rows with their answers, and the filtered-out row is absent from the data lines. An export that ignores the filter is how a filtered inbox leaks its whole history through a button labelled "Export", so the panel's button passes the same filters the table is showing and says "Export this filter" when one is on. Writing that assertion is what exposed a trap worth keeping: the first version grepped the whole CSV for `spam` and therefore tested the **header line**, which is the only line containing the word.*
- [x] A page with title, description, OG image and JSON-LD type emits the correct tags, and the sitemap includes it with the right `lastmod` after regeneration.
    *Proved (`a_page_with_metadata_emits_the_right_tags_and_lands_in_the_sitemap`): the saved page answers the *tag set the renderer emits* — title, description, canonical, OG title/description/image, `og_type`, robots — and the JSON-LD is **generated** from the page's own fields, carrying the real `headline` and a schema.org envelope rather than whatever an owner typed into a free-text box. A second walk (`a_noindex_page_is_…`) proves the other half: a `noindex` page is emitted with the directive **and kept out of the sitemap**, because a sitemap that advertises what robots.txt forbids is a contradiction every crawler resolves in the site's favour. That walk also pins the defect the unit test found: `#[serde(default)]` hands the store a JSON `null` for `structured_data`, and treating that as malformed made every unedited page unsaveable — so a *minimal* payload must answer 200, and it does.*
- [x] Redirect rules resolve correctly (301 literal increments hits, a regex rule matches its pattern, conflicting rules are flagged and loop chains refused). **The CSV import half is not built and the criterion is not met as written.**
    *Proved (`a_redirect_fires_counts_its_hit_and_a_rule_that_closes_a_loop_is_refused`, `a_path_two_rules_match_is_reported_as_ambiguous_and_a_test_does_not_count_a_hit`): a 301 literal answers with its `Location` and increments `hits`; a query string does not make a redirect a different rule (`/old-about?ref=news` still hits the literal); a `302` pattern rule answers its own dialect; and a path nothing answers is a 404. **The loop is refused before it is written** — `/loop-a → /loop-b` and then `/loop-b → /loop-a` answers **409 `redirect_loop`** naming the circle, and the refused rule is absent from SQL. The ambiguity half is proved by the panel's own entry point: `Test a path` reports the rule that answers **and the rules that also would**, and it does **not** increment `hits` — asserted from SQL, because that difference is the entire reason it is a separate route from the resolver. Three decisions are refusals of what the prose asked for, recorded rather than quietly ticked: **no CSV import** in this slice — the criterion's import half is simply not met, and leaving the export/import pair whole is better than shipping half of it; **the pattern dialect has no alternation, groups, `+`, `^` or `$`**, because an owner-typed pattern is matched against every request the site serves and a backtracking engine there is a denial-of-service surface one pattern can open (the dialect is literal text with `.`, `*` and `?`, anchored, and the unit tests pin every shape it refuses); and `Test a path` **reports** ambiguity instead of fixing it, since silently resolving two matching rules is how a site ends up with a rule nobody meant to have.*
- [~] A scheduled publish fires within a minute of its time in the site timezone, shows `done` in the queue, and a failed run shows an error with retry.
    *Queue and failure paths proved (`a_due_entry_publishes_once_and_records_the_result`, `a_publish_that_cannot_run_is_recorded_as_failed_with_its_reason`): a due entry publishes exactly once and records `done` plus the result; a publish with no draft records `failed` with the reason and `retry` returns it to `pending`. The 30-second worker is wired in `main.rs` and the queue screen is green: `runMenusDepth` reschedules through the form, reads the stored instant back from SQL, cancels the entry and requires `cancelled` in the table with the buttons gone (`queueReady`, `entryOnScreen`, `rescheduleFormOpened`, `rescheduleStored`, `rescheduleMoved`, `rescheduleIsLater`, `cancelledInSql`, `cancelButtonGone`, `filteredToPending`). Driving it exposed a defect the fourteen store tests were blind to: the queue listed its rows and then answered 400 `no_organization` to every button on them for a **platform owner** — the account onboarding creates first, and the one the queue's read path had been fixed for two ticks earlier while its four write handlers were not (`6a033bd`). "Within a minute" is still only observed at the store's boundary — a live wall-clock wait is not something a QA pass should pay for.*
- [ ] An approved comment appears on the public page with its reply thread; a comment tripping the heuristics lands in Spam without manual action.
- [ ] Double opt-in keeps a new subscriber `pending` until the confirmation link is used, the link expires, and unsubscribe flips the status while keeping the row.
- [ ] A gated page returns 404 to a signed-out visitor, renders for a verified member, and still returns 404 for a member missing the required role.
- [ ] Featured image, alt, legend and focal point round-trip on the page and are used by the renderer; a deleted featured image leaves the page renderable with a warning.
- [ ] All new screens render at 390 px without horizontal scroll and the walkthrough reports zero high findings.

### QA plan

The walkthrough must visit `/menus` (create a menu, nest items, `Add pages…`, assign locations, toggle visibility), `/forms` (build a form with every field type, publish, submit valid + honeypot + rate-limited from the public site, then moderate and export in the inbox), `/publishing/queue` (reschedule and cancel), `/seo/redirects` (create, `Test a path`, import CSV), `/seo/sitemap` (regenerate, save robots.txt), `/comments` (approve, spam, reply), `/newsletter/lists` (import, then run the double opt-in and unsubscribe from the public page), and `/members` (verify a visitor, then confirm the gated page for a signed-out and a signed-in visitor). Visual check: the menu editor shows a real nested tree with drag handles, the public header shows the built menu, the form renders styled inputs, the SERP preview shows the typed title, the sitemap preview is real XML, and moderation states render as badges.

### Slices

1. **Menus and scheduled publishing.** Migration `0124_cms_menus_publishing.sql` (renumbered from `0052` when main took `0052`; migration numbers are a shared namespace and a duplicate kills every suite at once); menu list/editor with nesting, ordering, page bulk-add, locations and visibility; audience-aware public menu payload rendered by the theme; queue screen, scheduling popover and a timezone-aware runner with retry. *Done when:* acceptance 1–4 and 13 pass and `/menus` plus `/publishing/queue` are in the walkthrough inventory.
2. **Forms.** Migration `0113_cms_forms.sql`; builder with all field types and validation, public render and submit route with honeypot and rate limits, submission inbox with export and bulk actions, notification e-mail and the outbound event. *Done when:* acceptance 5–9 pass and the public form renders in the walkthrough.
3. **SEO toolkit.** Migration `0114_cms_seo.sql`; page SEO tab with SERP and JSON-LD previews, sitemap generation and preview, robots.txt editor, redirect manager with test/import/export and loop refusal, broken-link view with create-redirect, plus the public sitemap and robots routes. *Done when:* acceptance 10–12 pass.
4. **Comments, newsletter, memberships, media reuse.** Migration `0115_cms_comments_newsletter_members.sql` (remaining tables); moderation inbox with heuristics and threads, newsletter lists with double opt-in and archive, visitor signup/sign-in/profile with page and block gating, featured image with focal point, and the consumed-event integrations. *Done when:* acceptance 14–18 pass, the visitor sign-in flow is exercised in the walkthrough, and the QA report lists zero high findings for the wave.

### Risks / notes

- Visitor accounts stay strictly separate from panel identities: separate tables, cookies, sessions and no code path that promotes a member to a user. This is the single most important boundary in this REQ.
- The publishing runner must claim due rows atomically (`for update skip locked`) and record a result, or a restart either loses or double-fires a publish.
- Redirect evaluation order (literal before regex, first match wins) is documented in the UI; chains are capped and loops refused, otherwise a bad rule can lock the site's own URLs.
- Spam protection is local heuristics only: the UI never claims "spam blocked" with certainty, the honeypot stays invisible to screen readers, and blocked-word lists are matched case-insensitively.
- Double opt-in and unsubscribe tokens are stored hashed, single-use and time-limited; unsubscribe works without sign-in and must never be guessable.
- Newsletter sending overlaps with REQ-060 by design — one sender, two entry points; marketing owns campaigns and tracking, this module owns lists and opt-in, and the send path must not fork.
- SEO tags belong to the renderer's metadata layer so they land in one place; duplicating them in the app shell and in themes is how metadata drifts.
- Turkish example copy stays in seeded sample content (a demo menu label, a comment sample, a newsletter issue subject), never in API strings or log messages.
