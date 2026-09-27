-- Omnion · 0026 · AI model capabilities and the discovery diff (REQ-097 slice 2)
--
-- 0008 registered a model with four flags: tools, vision, streaming and embeddings. REQ-097 asks
-- for the full typed set — "chat, stream, embeddings, image generation, audio, transcription,
-- list-models" plus tools, vision and JSON mode — and for the flags to be *data the registry
-- edits and the router enforces*, so no caller guesses what a model can do.
--
-- Slice 1 (0022) widened the provider side. This migration widens the model side, and adds the
-- answer ceiling a model advertises. Released migrations are append-only (docs/05-VERSIONING.md):
-- 0008 keeps its four flags and the four new ones arrive beside them, all `default false` so a
-- row that has never been reviewed claims nothing beyond what it was registered with.

-- Image generation (the platform declares and enforces it; the execution path is REQ-097's
-- "out" list and answers `not_supported` until a later request ships it).
alter table ai_models
    add column supports_image_generation boolean not null default false;

-- Audio generation, same shape.
alter table ai_models
    add column supports_audio_generation boolean not null default false;

-- Transcription, same shape.
alter table ai_models
    add column supports_transcription boolean not null default false;

-- Structured output through the provider's own JSON mode. This is the flag a caller needs when
-- it wants a parsed object back rather than prose, and it is refused by the router when false.
alter table ai_models
    add column supports_json_mode boolean not null default false;

-- The largest answer the model will produce, when the provider advertises one. `null` means "the
-- registry does not know", which is a different fact from "0" — a zero-length model cannot
-- answer, and a row that does not know may still answer a lot.
alter table ai_models
    add column max_output_tokens integer;

alter table ai_models
    add constraint ai_models_max_output_tokens_check
    check (max_output_tokens is null or max_output_tokens > 0);

-- Discovery compares what an endpoint reports against what is stored, and the reported list is
-- read per provider in key order; the existing key is unique per provider, so the lookup the diff
-- does is already indexed. The (provider_id, model_key) pair the diff writes on is served by
-- ai_models_provider_key_key, and the registry list reads `order by model_key`, so this index
-- covers the panel's per-provider model table without a sort.
create index ai_models_provider_key_idx on ai_models (provider_id, model_key);
