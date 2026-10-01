-- Give the webhook channel a destination it can actually POST to.
--
-- **The bug this migration exists for.** `notification_deliveries` was shipped with slice 4
-- and the webhook transport shipped with it, and the transport posts `job.url` — which is the
-- notification's *in-app* deep link (`/settings/iam/sessions`, `/media/files/{id}`). Those are
-- relative paths: `reqwest` cannot send them, so every webhook delivery failed with
-- `builder error: relative URL without a base`, burned three attempts and landed in `failed`.
-- The outbox showed a queue of red rows with a reason no reader could act on, and the fix that
-- looked obvious — giving the notification a real URL — is the wrong one, because the in-app
-- link is what the bell deep-links into and changing it would break the in-app channel to fix
-- the webhook one.
--
-- So the destination moves to the channel's own row, which is where a transport credential
-- already lives and where a multi-tenant install has to put it anyway: `notification_channels`
-- is per organization, so the endpoint cannot come from the process config (that would send
-- every organization's notifications to one URL).
--
-- **A url column and not a new table.** One webhook channel per organization is already the
-- unique constraint on `notification_channels`, so a second table would hold at most one row
-- and enforce nothing the column does not.
--
-- **`endpoint_id` points at the existing webhook endpoint, and `endpoint_url` is the
-- fallback.** The bus already owns endpoints with signing secrets (`webhook_endpoints`,
-- REQ-016) and this module refuses to keep a second copy of a secret; so the column stores a
-- reference when the organization pointed the channel at one of its own endpoints, and the
-- literal URL when it did not (a channel pointed at an external collector that has no bus
-- endpoint). Both are nullable and exactly one is expected — the check below refuses the row
-- that has neither, because "configured" with nowhere to send is the state that produced the
-- failures above.
--
-- Adding the column is additive: existing rows read `NULL` for both, and
-- `channel_readiness`'s webhook branch is changed with it (see the code change) so the
-- settings screen says *why* the channel cannot send instead of reporting the one line that
-- was never true.
alter table notification_channels
    add column endpoint_id uuid references webhook_endpoints (id) on delete set null,
    add column endpoint_url text,
    add constraint notification_channels_webhook_destination_check
        check (
            channel <> 'webhook'
            or endpoint_id is not null
            or endpoint_url is not null
        ),
    add constraint notification_channels_endpoint_url_check
        check (
            endpoint_url is null
            or endpoint_url ~ '^https?://[^[:space:]]+$'
        );

comment on column notification_channels.endpoint_id is
    'the webhook_endpoints row this channel posts to (REQ-016), or null when it posts to endpoint_url instead';
comment on column notification_channels.endpoint_url is
    'a literal https URL for the webhook channel, used only when no bus endpoint is referenced';

-- The channel's destination is per organization, so it has to be readable by one. This index
-- is what lets `claim_due` resolve the destination for a claimed row without a sequential scan
-- over every organization's channels: the join is `n.organization_id -> notification_channels`.
create index notification_channels_org_channel on notification_channels (organization_id, channel);
