-- 0217_security_ip_rules.sql — the allow/deny lists (REQ-012, slice 4).
--
-- The last piece of the request's own security surface that had no table: an operator can deny
-- a CIDR on the panel and the platform will keep serving it. The spec asks for "IP allow/deny
-- lists with CIDR matching, expiry and an evaluator that runs before route guards", and every
-- previous slice of this REQ shipped a model, a screen and a route without the one thing that
-- makes an access list real — somewhere for the rule to live.
--
-- The shape decisions, each because of a specific way an access list goes wrong:
--
--   * `cidr` is the **`inet` type**, not `text`. Postgres knows how to compare an address to a
--     range (`<<=`), so the evaluator's match is a typed comparison rather than a string
--     comparison that a typo (`10.0.0.0/8 ` with a trailing space, or `10.0.0.0/8x`) silently
--     fails to match. A list stored as text and parsed per request is a list whose CIDR syntax
--     errors are only ever discovered by the operator who typed the broken one.
--   * The **prefix length is constrained, not the address.** `cidr` in Postgres rejects a bare
--     address and an over-long prefix at insert time with a message that names the input, so a
--     malformed rule cannot reach the table at all — which is the acceptance criterion ("CIDR
--     validation rejects malformed input (IPv4 and IPv6) with a field-level message"), satisfied
--     at two layers: the Rust parser gives the message the form shows on the field, and this
--     column is the backstop that holds when a caller bypasses the route.
--   * `kind` is a **closed list in SQL**, matching `RATE_SCOPES`' arrangement: a kind the panel
--     cannot render is a rule the evaluator would honour and the operator could never see or
--     remove. The list is `allow`/`deny` and nothing else.
--   * **Deny is not modelled as "allow wins with a negative weight" or as a priority column.**
--     "Deny always wins" is a *rule about two rules*, and it belongs in the evaluator, where the
--     two rules are both in hand at once. Encoding it here would mean the precedence is decided
--     by whatever the database returns first.
--
-- On `unique (kind, cidr)`: the same network can legitimately appear on both lists only as a
-- mistake, and `ipnet`'s parser normalises the network address, so `10.0.0.0/8` and `10.1.2.0/8`
-- are the same row rather than two rules that match different addresses depending on which
-- string reached `contains`. The uniqueness is per kind, so an operator can allow a range that
-- is also denied — the evaluator answers that case with the deny, which is the documented
-- precedence and is why both rows are permitted to exist at all.

create table if not exists security_ip_rules (
    -- PostgreSQL's own `cidr` input: rejects a bare address, a host bit set outside the prefix,
    -- and a prefix outside 0–32 / 0–128, by name.
    cidr cidr not null,
    -- `allow` or `deny`. A closed list, because a rule the panel cannot render is a rule the
    -- evaluator would still honour.
    kind text not null,
    -- Why the rule exists. Free text, but never empty: an unexplained access rule is one an
    -- operator removes without reading, or leaves in place without understanding.
    note text not null default '',
    created_by uuid references users (id) on delete set null,
    created_at timestamptz not null default now(),
    -- An expiry makes this a temporary block rather than a permanent one, which is what an
    -- operator reaches for during an incident and then forgets about. `null` never expires.
    expires_at timestamptz,
    id uuid primary key default gen_random_uuid(),

    constraint security_ip_rules_kind check (kind in ('allow', 'deny')),
    constraint security_ip_rules_note_not_blank check (length(btrim(note)) > 0),
    constraint security_ip_rules_expiry_after_creation check (
        expires_at is null or expires_at > created_at
    )
);

-- One rule per network per list. `cidr` is a range type, so this compares networks rather than
-- the text that named them: `10.0.0.0/8` twice is one row, and a differently-spelled but
-- identical network is refused instead of silently doubling up.
create unique index security_ip_rules_kind_cidr_uniq on security_ip_rules (kind, cidr);

-- The evaluator's only query is "every live rule", so the index that matters is on the list
-- order rather than on the address: there is no per-address lookup, because the answer is a
-- scan over a table that holds a handful of rows. The partial predicate on `expires_at` is
-- deliberately NOT part of it — an expired rule still has to be *returned* so the evaluator can
-- report "the rule that matched has expired" instead of silently answering "no rule matched",
-- which would read as "allowed" for a deny that an operator believes is in force.
create index security_ip_rules_kind_created_idx on security_ip_rules (kind, created_at desc);

-- The screen shows "Added" and "Added by", and a rule list sorted newest-first is what an
-- operator reads during an incident, so both orders are available without a sort.
comment on table security_ip_rules is
    'Allow/deny CIDR rules for the security centre (REQ-012 slice 4). Deny wins over allow; an expired rule is skipped but still reported by the tester.';
comment on column security_ip_rules.cidr is
    'Typed network. Postgres validates the CIDR syntax and the prefix length, so a malformed rule cannot be stored.';
comment on column security_ip_rules.kind is
    'allow | deny. Deny always wins when a single address matches both lists.';