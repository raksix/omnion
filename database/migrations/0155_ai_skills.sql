-- REQ-099 · Agent runtime & tool loop — slice 3: the skills registry.
--
-- A skill is **data, never code**: a validated, versioned definition (key, name, description,
-- when-to-use, instructions, and an optional list of tool keys it may reach). Nothing here can
-- execute. That is the whole security argument for the feature, so it is worth stating where the
-- table is created: `instructions` is data that ends up inside a prompt, and the *only* thing it
-- can cause is the tool calls the agent's allow-list already permits.
--
-- Four decisions here are load-bearing, and each of them exists because the obvious alternative
-- fails a real case:
--
-- 1. **`organization_id` is nullable, and NULL means built-in.** A built-in skill is shared by
--    every organization on the installation; a custom one belongs to exactly one. Modelling
--    "built-in" as a boolean column instead would mean every read has to remember to exclude
--    the flag, and the one query somebody forgets to filter leaks the whole seed set into a
--    tenant's registry. NULL carries the meaning in the row itself.
--
-- 2. **Uniqueness is a folded index, not a plain unique.** `unique (organization_id, key)` does
--    NOT work here, and this is the classic PostgreSQL trap: in a regular unique constraint NULLs
--    are considered distinct, so every organization could register a built-in-shaped row with
--    `organization_id = NULL` and the index would happily hold all of them. `unique index ...
--    on (coalesce(organization_id, '00000000-0000-0000-0000-000000000000'::uuid), key)`
--    collapses NULL to one sentinel so the constraint is real. (This is also why the sentinel
--    cannot be a value an organization can ever hold: it is the nil uuid, which is refused as a
--    foreign key elsewhere in the schema.)
--
-- 3. **`checksum` is over the definition, and the runtime recomputes it.** A skill row that was
--    edited in the database — by a migration, a restore, or an operator with psql — would
--    otherwise be injected into a prompt while claiming an integrity value nobody verified. The
--    runtime compares a freshly computed digest against the stored one at *assemble* time and
--    refuses a mismatch, so a tampered row cannot reach a model even if every application-level
--    check was bypassed.
--
-- 4. **`ai_agent_skills` is ordered and cascade-deletes.** The position is the runtime order the
--    spec promises ("injects enabled skills into the agent's prompt in attached order"), and a
--    row that is re-ordered is an update of `position` rather than a delete/insert pair, so the
--    audit trail keeps one row per attachment.

create table if not exists ai_skills (
    id              uuid primary key default gen_random_uuid(),
    -- NULL is the built-in marker. See decision (1).
    organization_id uuid        references organizations (id) on delete cascade,
    key             text        not null,
    name            text        not null,
    description     text        not null default '',
    when_to_use     text        not null default '',
    instructions    text        not null,
    tools           jsonb       not null default '[]'::jsonb,
    version         integer     not null default 1,
    checksum        text        not null,
    source          text        not null default 'custom',
    enabled         boolean     not null default true,
    created_by      uuid        references users (id) on delete set null,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),

    constraint ai_skills_source_known check (source in ('built_in', 'custom')),
    constraint ai_skills_version_positive check (version >= 1),
    constraint ai_skills_key_shape check (key ~ '^[a-z][a-z0-9_-]{0,63}$'),
    constraint ai_skills_name_len check (char_length(name) between 1 and 80),
    constraint ai_skills_description_len check (char_length(description) <= 200),
    constraint ai_skills_when_to_use_len check (char_length(when_to_use) <= 500),
    constraint ai_skills_instructions_present check (char_length(instructions) between 1 and 8000),
    constraint ai_skills_checksum_shape check (checksum ~ '^[0-9a-f]{64}$'),
    -- A built-in has no organization; a custom skill always does. Without this, a row could be
    -- both — invisible to the tenant listing and editable by nobody.
    constraint ai_skills_source_scope check (
        (source = 'built_in' and organization_id is null)
        or (source = 'custom' and organization_id is not null)
    )
);

-- Decision (2). A plain `unique (organization_id, key)` would not fire for the NULL row.
create unique index ai_skills_org_key_unique
    on ai_skills (coalesce(organization_id, '00000000-0000-0000-0000-000000000000'::uuid), key);

-- The registry list: enabled rows first, then by key, for one organization *and* the built-ins.
create index ai_skills_registry_idx on ai_skills (enabled, key);
create index ai_skills_org_idx on ai_skills (organization_id) where organization_id is not null;

-- The attachment. `skill_key` is a plain text reference rather than a uuid foreign key on
-- purpose: an agent can hold a skill that a later edit removed from the registry, and the
-- attachment must survive that so the Skills tab can say "stale" instead of silently losing the
-- row. The runtime refuses a stale or checksum-mismatched skill rather than injecting it.
create table if not exists ai_agent_skills (
    agent_id   uuid        not null references ai_agents (id) on delete cascade,
    skill_key  text        not null,
    position   integer     not null default 0,
    attached_by uuid       references users (id) on delete set null,
    attached_at timestamptz not null default now(),

    primary key (agent_id, skill_key),
    constraint ai_agent_skills_position check (position >= 0),
    constraint ai_agent_skills_key_shape check (skill_key ~ '^[a-z][a-z0-9_-]{0,63}$')
);

-- The runtime reads an agent's skills in attachment order; this is the index that makes it a
-- plain ordered scan rather than a sort.
create index ai_agent_skills_order_idx on ai_agent_skills (agent_id, position);
-- "Which agents use this skill?" — the registry's "Used by" column.
create index ai_agent_skills_key_idx on ai_agent_skills (skill_key);

-- The built-in seed. Three skills, because a seed of one teaches nothing and a seed of twenty
-- is a scroll of noise. They are written with `on conflict do nothing` so a boot that runs twice
-- does not fail on the folded unique index, and with an explicit organization_id of NULL.
--
-- The checksums below are the values `skills::checksum_of` produces for these exact bodies,
-- printed by `cargo run -p omnion-ai-hub --example seed_checksums` rather than typed by hand —
-- a hand-written digest is a value nobody can check, and the runtime refuses a skill whose
-- checksum does not describe its own body, so a typo here would make the whole seed unusable
-- and report it as "checksum does not match" on three rows that look perfectly well-formed.
-- `the_seed_checksums_describe_their_own_bodies` recomputes all three in a test, so a body
-- edited here without rerunning the example fails the build rather than the installation.
insert into ai_skills (key, name, description, when_to_use, instructions, tools, source, enabled, checksum)
values
    (
        'summary',
        'Summarise',
        'Condense a long document or transcript into the points that matter.',
        'Use when the user asks for a summary, a digest, or "the short version" of something long.',
        E'Thread of intent: the final answer names what this text is about, not what it says.\n\nMethod:\n1. Read the whole input before writing anything. A summary of the first half is a\n   summary of a different document.\n2. Keep the claims that carry decisions, numbers, names and dates. Drop the connective\n   tissue — transitions, restatements and throat-clearing.\n3. Preserve disagreement: if the source contradicts itself, say so rather than picking a side.\n4. Mark anything the source asserts without support as an unverified claim.\n\nLength: aim for one tenth of the input, and never return more than the source contains.',
        '[]'::jsonb,
        'built_in',
        true,
        '75ff0dfc770eec656b189a43e5c06f23fe46b0d0d300fd14efe35718bd467622'
    ),
    (
        'citation',
        'Cite sources',
        'Ground every factual claim in a numbered source, or say that none exists.',
        'Use when the answer asserts facts about the world that the user may need to verify.',
        E'Every factual sentence carries a bracketed number, like [1], pointing at the numbered\nsource list you end with.\n\nRules:\n1. Cite the sentence that makes the claim, not the paragraph it sits in. A claim with no\n   support in any source is the one thing this skill exists to prevent.\n2. If no source supports a claim, either drop the claim or write it as\n   "unverified — no source found". Never manufacture a citation to fill the gap.\n3. A source is only a source if you read it. A title you recognise is not a source.\n4. When sources disagree, cite both and state the disagreement rather than silently choosing.\n\nEnd with the numbered list of what you actually read.',
        '[]'::jsonb,
        'built_in',
        true,
        '64e76225545ddc708b0a4d67e470e8d5cd868f1396936c6538505c62941b68fd'
    ),
    (
        'tool-discipline',
        'Tool discipline',
        'Look before you leap: prefer one well-formed tool call over several guessed ones.',
        'Use whenever the agent holds tools — prefer it over improvising a call shape.',
        E'A tool call is an operation on somebody''s system, not a guess about one.\n\n1. If a tool needs an argument you do not have, ask for it in plain text. Do not invent a\n   value, and do not call the tool to see what error it returns.\n2. One call at a time. A second call that depends on the first''s result waits for it.\n3. If a call fails, read the error before retrying. Retrying an identical call after an error is\n   a loop, and the loop detector will end the run before you learn anything.\n4. Quote the tool''s own result when you rely on it. Never assert what a tool returned when it\n   returned an error, and never smooth over a partial result into a confident summary.',
        '[]'::jsonb,
        'built_in',
        true,
        'c462c4c6e5f3fdbb164c5119569f4bc4cef7c317f6166777dcf91086c26a573a'
    )
on conflict do nothing;
