-- REQ-052 slice 6: a provider registered after `search_settings` was written is silently off.
--
-- The settings row stores `enabled_providers` as a text array, and `read_settings` only falls
-- back to the full registry when the row is **absent**. So the first installation to have its
-- settings written — which is every installation, because the settings screen writes them on
-- first save — froze the list of providers that existed that day. Adding a provider to the
-- registry afterwards therefore does nothing: the indexer upserts its rows on every reindex, the
-- status screen lists it, and the query filters it out with `d.provider = any(enabled)`.
--
-- That is the same shape as REQ-051's three CRM providers, one level deeper: those were missing
-- from the panel registry, these were missing from a **data row**. Neither produces an error. The
-- walk in `apps/api/tests/search_business_providers.rs` reports fourteen indexed rows and zero
-- hits, which is the only honest symptom.
--
-- The rule this migration establishes: **a provider nobody has explicitly turned off is on.** A
-- key already in the row stays — an operator's deliberate choice to search fewer providers is not
-- overridden by a platform upgrade — and the list is then restricted to keys the registry still
-- knows, so a provider that was renamed or withdrawn does not sit in the row forever filtering
-- nothing.
--
-- Written as a union, so re-running it is a no-op: an operator who disabled a provider before
-- this migration ran keeps it disabled. What it deliberately does **not** do is turn anything back
-- on later; that is the settings screen's job.
--
-- The `array_agg` sits **outside** the union, one level up, because `array_agg` aggregates the
-- union's rows and a union whose arms are themselves arrays is a type error rather than a
-- concatenation. The inner query therefore projects a flat `text` column and the outer one turns
-- those rows back into an array — the same shape the CRM providers would have needed.

update search_settings
set enabled_providers = (
        select coalesce(array_agg(provider order by provider), '{}'::text[])
        from (
            select existing.provider
            from unnest(enabled_providers) as existing(provider)
            where existing.provider in (
                'pages', 'media', 'users', 'sites', 'logs', 'translations', 'settings',
                'contacts', 'companies', 'deals', 'quotes', 'orders'
            )
            union
            select provider
            from unnest(array['quotes', 'orders']::text[]) as added(provider)
        ) as merged
    ),
    updated_at = now()
where id = 1
  and not (
      'quotes' = any (enabled_providers)
      and 'orders' = any (enabled_providers)
  );
