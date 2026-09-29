-- Omnion · 0125 · content: the form builder and its submission inbox (REQ-064, slice 2)
--
-- A contact form is the oldest thing a CMS owes its owner, and the obvious shape — one table of
-- fields and one table of rows — is wrong in three places, each of which this migration answers
-- in the schema rather than in application code.
--
-- 1. **A form is site-scoped, and its `key` is its public address.** `/f/{site_key}/{form_key}`
--    is what a theme embeds and what a visitor's browser posts to, so the pair is unique and the
--    key is validated by the same rules a page slug is. Organization-scoped forms would let one
--    site's inbox receive another site's submissions.
--
-- 2. **Field definitions are rows, not JSON.** A builder drag-and-drops fields, and each one
--    carries its own validation. Keeping the definition in `jsonb` on the form would make
--    "which fields are required" a full scan of every submission, and would make a field rename
--    a data migration instead of an edit. The rules and options stay JSON *inside the field*,
--    because those are per-field, free-shaped and never queried.
--
-- 3. **A submission stores its answers as JSON and its consent text verbatim.** The shape of an
--    answer is the field's shape, and a form that gains a field must not require rewriting every
--    historical row. What is NOT free-shaped is who sent it and what it was about, so `ip_hash`,
--    `user_agent_hash`, `source_path` and `spam_score` are columns — the rate limiter and the
--    spam heuristics both need them on every submission, and a JSON scan of the inbox for an
--    index is a scan.
--
-- The consent text is stored as the *text that was shown*, not a boolean. A row saying "yes"
-- cannot answer "what did they agree to?" a year later, which is the only question that matters.

create table if not exists cms_forms (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    site_id uuid not null references sites (id) on delete cascade,
    key text not null,
    name text not null,
    status text not null default 'draft',
    submit_action text not null default 'message',
    submit_message text,
    redirect_url text,
    notify_emails text[] not null default '{}',
    notify_subject text,
    honeypot boolean not null default true,
    min_fill_seconds integer not null default 3,
    rate_limit_per_hour integer not null default 20,
    retention_days integer not null default 365,
    target_segment_id uuid,
    created_by uuid references users (id) on delete set null,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    constraint cms_forms_key_format check (key ~ '^[a-z0-9]+(-[a-z0-9]+)*$'),
    constraint cms_forms_status check (status in ('draft', 'published')),
    constraint cms_forms_submit_action check (submit_action in ('message', 'redirect')),
    constraint cms_forms_submit_message_required check (
        submit_action <> 'message' or submit_message is not null
    ),
    constraint cms_forms_redirect_required check (
        submit_action <> 'redirect' or (redirect_url is not null and redirect_url <> '')
    ),
    constraint cms_forms_min_fill_seconds check (min_fill_seconds between 0 and 3600),
    constraint cms_forms_rate_limit check (rate_limit_per_hour between 1 and 10000),
    constraint cms_forms_retention_days check (retention_days between 1 and 3650),
    unique (site_id, key)
);

create table if not exists cms_form_fields (
    id uuid primary key default gen_random_uuid(),
    form_id uuid not null references cms_forms (id) on delete cascade,
    position integer not null default 0,
    key text not null,
    label text not null,
    field_type text not null,
    required boolean not null default false,
    placeholder text,
    help_text text,
    width text not null default 'full',
    rules jsonb not null default '{}'::jsonb,
    options jsonb not null default '[]'::jsonb,
    constraint cms_form_fields_key_format check (key ~ '^[a-z0-9_]+$'),
    constraint cms_form_fields_type check (
        field_type in ('text', 'textarea', 'select', 'radio', 'checkbox', 'date', 'file', 'consent')
    ),
    constraint cms_form_fields_width check (width in ('half', 'full')),
    unique (form_id, key)
);

create index if not exists cms_form_fields_position_idx on cms_form_fields (form_id, position);

create table if not exists cms_form_submissions (
    id uuid primary key default gen_random_uuid(),
    form_id uuid not null references cms_forms (id) on delete cascade,
    site_id uuid not null references sites (id) on delete cascade,
    answers jsonb not null default '{}'::jsonb,
    consent_text text,
    source_path text,
    ip_hash text,
    user_agent_hash text,
    spam_score integer not null default 0,
    status text not null default 'new',
    created_at timestamptz not null default now(),
    constraint cms_form_submissions_status check (status in ('new', 'read', 'spam', 'archived')),
    constraint cms_form_submissions_spam_score check (spam_score between 0 and 100)
);

-- The inbox reads one form at a time, newest first; the rate limiter counts the same form by
-- sender over a moving hour. Two indexes, two different questions.
create index if not exists cms_form_submissions_inbox_idx
    on cms_form_submissions (form_id, created_at desc);

create index if not exists cms_form_submissions_rate_idx
    on cms_form_submissions (form_id, ip_hash, created_at desc);
