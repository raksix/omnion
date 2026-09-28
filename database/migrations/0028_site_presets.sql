-- Omnion · 0027b · media: a new site gets its `standard` preset
--
-- The `0027` seed gives every site that existed when it ran a `standard` transformation preset.
-- A site created *afterwards* gets nothing, and a page whose markup already asks for
-- `?preset=standard` would then silently fall back to full-size originals on that site.
--
-- The bug lives in the gap between "the migration ran" and "somebody creates a site", so it
-- cannot be fixed in the migration. It is fixed here, as a trigger, because there is no single
-- code path that creates a site: the onboarding flow, the tenancy API and a future import all
-- insert the row themselves, and each one would have to remember. A trigger is the only place
-- that is guaranteed to see every site.
--
-- The function is deliberately not a function at all: this is a plain `after insert` trigger with
-- an `on conflict do nothing`, so it is idempotent, it never fails a site creation over a preset
-- row, and it needs no dependency from `crates/identity` to `crates/media`.

create or replace function omnion_seed_site_presets() returns trigger
language plpgsql
as $$
begin
    insert into media_transformation_presets
        (site_id, name, width, height, fit, format, quality)
    values
        (new.id, 'standard', 1200, 630, 'cover', 'webp', 80)
    on conflict (site_id, name) do nothing;
    return new;
end;
$$;

comment on function omnion_seed_site_presets() is
    'Gives every new site the `standard` transformation preset, so a page that already asks for '
    '?preset=standard never falls back to a full-size original (REQ-010).';

drop trigger if exists sites_seed_presets on sites;
create trigger sites_seed_presets
    after insert on sites
    for each row execute function omnion_seed_site_presets();
