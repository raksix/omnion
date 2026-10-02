-- Omnion · 0117 · The attribute map gets a table of its own (REQ-065, slice 2).
--
-- Slice 1 widened the provider *registry* (0051). The wizard's third step — external attribute →
-- panel field, with a transform and a required flag — has nowhere to live: `config.role_mappings`
-- already carries the claim → role rules from REQ-006, and putting a second, differently-shaped
-- document in the same JSON blob is how "edit the attribute map" ends up rewriting the role rules
-- nobody was looking at. So the map is a table: one row per target field, ordered, and replaced
-- atomically by a single PUT.
--
-- Why the shape is what it is:
--
-- * `unique (provider_id, target_field)` — the panel can only send one source attribute per field,
--   and a duplicate is a silent "which one wins?" at provisioning time, where the losing row
--   produces a half-filled account. The constraint turns that into a 400 at save time.
-- * `transform` + `transform_arg` are a pair rather than one string. `static` needs an argument,
--   `lowercase` does not, and a single string would either carry an argument that means nothing for
--   most transforms or be a second lookup table for the one that does.
-- * `required` is about the *panel field*, not the external attribute: "an account cannot exist
--   without an email", which is a platform rule that happens to be satisfied by a claim. A missing
--   required source is refused by name at save time and at preview time.
-- * `position` is explicit rather than derived from an insertion timestamp, because drag order in
--   the editor is the operator's mental model and two rows sharing a timestamp order by id, which
--   is not what anybody meant.

create table provider_attribute_mappings (
    id uuid primary key default gen_random_uuid(),
    provider_id uuid not null references auth_providers (id) on delete cascade,
    source_attr text not null,
    target_field text not null
        check (target_field in (
            'email', 'username', 'display_name', 'phone',
            'department', 'title', 'employment_type', 'employee_id'
        )),
    transform text not null default 'none'
        check (transform in ('none', 'trim', 'lowercase', 'prefix', 'static', 'split')),
    transform_arg text,
    required boolean not null default false,
    position integer not null default 0,
    created_at timestamptz not null default now()
);

-- One source attribute per panel field, per provider.
create unique index provider_attribute_mappings_field_idx
    on provider_attribute_mappings (provider_id, target_field);

-- The editor reads one provider's map in order; the preview reads the same rows.
create index provider_attribute_mappings_provider_idx
    on provider_attribute_mappings (provider_id, position);
