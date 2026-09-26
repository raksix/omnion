# REQ-102 — AI Memory & Knowledge Base (RAG)

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/ai-hub`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Giving the AI your context.

- Memory scopes: global, organization, site, user, conversation — read/write rules per scope.
- Knowledge sources: pages, media text, uploaded documents, external URLs; indexing pipeline.
- Chunking strategies, embeddings, vector store, reindex on content change.
- Retrieval with citations back to the source page/document.
- "Site AI context" so copilots answer in the context of the current site/brand.

## Implementation spec

### Scope (in / out)

**In** — extends the `crates/ai-hub` surface of REQ-001 (providers, registry, router, knowledge
collections, `ai_memory`) with the two halves that make an assistant useful in a tenant: what it
remembers, and what it knows.

- **Memory model** — one row per fact, five scopes in a single table rather than five tables. A
  memory row is a `key` plus a jsonb `value`, never a transcript (transcripts belong to REQ-103).
- **Scope rules**, enforced in the service and asserted in tests: `global` rows are readable by
  every authenticated caller and writable only by an operator with `ai.memory.manage` and no
  organization; `organization` rows are readable by organization members; `site` rows follow the
  caller's site grant; `user` rows are readable only by their `user_id`; `conversation` rows live
  and die with the conversation they belong to.
- **Knowledge sources** — four kinds, each normalized into a source row plus an extraction step:
  page revisions, media text (caption, alt text, and the extracted text of a document media item),
  uploads (md/txt/csv/html/json plus pdf/docx with a readable text layer), and external URLs
  (fetched server-side on an explicit action, stored as extracted text plus fetch time).
- **Retrieval** — `vector` (cosine) or `hybrid` (vector plus full-text `tsvector` ranking in the
  spirit of the search crate) with `top_k`, `min_score`, and an optional rerank step using any
  model flagged `supports_rerank`. Hard rule: an answer built from knowledge carries citations,
  and a chunk whose source no longer resolves is dropped from the prompt instead of cited.
- **Reindex on change** — a published page revision, changed media text, replaced upload, or a
  change to a collection's chunking/embedding settings enqueues an indexing job. Deleting a source
  deletes its chunks, in the same transaction where the caller allows it and by the runner
  otherwise.
- **Site AI context** — one editable profile per site (brand name, tone, primary language, target
  audience, extra instructions) injected into every request that carries the site in scope, plus
  the site's default collection set.

**Out**

- Training, fine-tuning and any write-back into a model — docs/06 §12–13 stop at retrieval.
- Hosted vector databases and embedding endpoints beyond a configured provider; the store stays in
  PostgreSQL.
- A general crawler: an external URL is indexed one document at a time, on an explicit action.
- Conversation storage/sharing (REQ-103), the cost ledger beyond the token rows indexing writes
  (REQ-104), and PII handling of extracted text (REQ-105 owns masking).

### Screens (UI)

- **`/ai/memory`** — table: Scope, Key, Value (preview ≤60 chars, expandable), Site/User, Kind,
  Source, Pinned, Updated. Filters: scope, kind, site, user, free-text key/value; "expired" toggle.
  Row actions Edit, Pin/Unpin, Delete (confirm by key). Header action "New memory" opens a form:
  Scope (select, options limited by the caller's permissions), Target (site/user/conversation
  picker, shown per scope), Key (1–80, `[a-z0-9_.-]`), Kind, Value (jsonb editor with a plain-text
  mode), Pinned, Expires at (optional, must be in the future). Empty state "Nothing remembered
  yet" with a "Add a fact" action; `LoadingTable` skeleton; error banner with the API message and
  Retry.
- **`/ai/knowledge`** — collections table: Name, Sources, Chunks, Embedding model, Retrieval,
  Last indexed, Status. Filters name, embedding model, status. Create form: Name (1–80, unique per
  organization), Description (≤400), Embedding model (required, only `supports_embeddings` models
  offered), Chunk strategy + parameters, Retrieval mode, Top k (1–50).
- **`/ai/knowledge/[collection]`** — tabs **Sources**, **Search**, **Settings**. Sources table:
  Title, Kind, Language, Chunks, Status, Indexed at, Error; add-source drawer with four tabs (Page
  revision picker, Media picker, Upload, URL) — the URL tab offers Inspect before Add and shows the
  fetched title, byte size and language. Search tab: query box, mode and top-k controls, ranked
  results with score, heading path, source link and a "Copy citation" action. Settings tab holds
  chunking/embedding/retrieval fields and the danger zone (Reindex all, Delete collection).
- **Keyboard** — `/` focuses the screen search, `N` new row, `E` edit the focused row, `R` reindex
  the focused source, `↑/↓` + `Enter` move and open, `Esc` closes the drawer; focus returns to the
  invoking row.
- **Mobile (<1024px)** — tables become label/value cards, filters collapse into a sheet, the
  add-source drawer becomes a full-screen sheet with one tab per step, the citation drawer opens as
  a bottom sheet. No action is hover-only.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/ai/memory` | List memory (`scope`, `kind`, `site_id`, `user_id`, `q`, cursor) | `ai.memory.read` |
| POST | `/api/v1/ai/memory` | Write one memory row in a permitted scope | `ai.memory.manage` |
| PATCH/DELETE | `/api/v1/ai/memory/{id}` | Edit or delete a row | `ai.memory.manage` |
| GET/PUT | `/api/v1/ai/sites/{id}/context` | Read / replace the site AI context | `ai.memory.read` / `ai.memory.manage` |
| GET/POST | `/api/v1/ai/knowledge/collections` | List / create collections | `ai.knowledge.read` / `ai.knowledge.manage` |
| PATCH/DELETE | `/api/v1/ai/knowledge/collections/{id}` | Chunking, retrieval, embedding model | `ai.knowledge.manage` |
| POST | `/api/v1/ai/knowledge/collections/{id}/sources` | Attach a source and queue indexing | `ai.knowledge.manage` |
| DELETE | `/api/v1/ai/knowledge/sources/{id}` | Remove a source with its chunks | `ai.knowledge.manage` |
| POST | `/api/v1/ai/knowledge/sources/{id}/reindex` | Re-extract, re-chunk, re-embed one source | `ai.knowledge.manage` |
| GET | `/api/v1/ai/knowledge/search` | Ranked chunks with citations (`q`, `mode`, `top_k`) | `ai.knowledge.read` |
| POST | `/api/v1/ai/knowledge/ask` | Answer from knowledge with citations (SSE) | `ai.chat` + `ai.knowledge.read` |
| POST | `/api/v1/ai/knowledge/urls/inspect` | Fetch a URL and report title, size, language | `ai.knowledge.manage` |

New catalogue keys: `ai.memory.read`, `ai.memory.manage`. Writing a `global` row needs the operator
scope in addition, and the API answers 403 naming the missing scope.

### Data model

Migration `database/migrations/00NN_ai_memory.sql` (00NN = next free integer at land time; 0016 was
free when this was written — released migrations are append-only). It extends the REQ-001 tables:
`ai_knowledge_collections` gains `chunk_strategy` text default `'tokens'`, `chunk_params` jsonb
default `'{}'`, `retrieval_mode` text default `'vector'`, `top_k` int default 8, `min_score`
numeric(4,3) default 0.100, `rerank_model_id` uuid null; `ai_knowledge_documents` gains
`content_hash` text and `heading_count` int; `ai_knowledge_chunks` gains `heading_path` text,
`source_snapshot` jsonb, and `search tsvector generated always as (to_tsvector('simple', content))
stored` with a GIN index.

| Table | Columns (types) | Indexes |
|---|---|---|
| `ai_memory` (extends REQ-001) | id uuid pk, scope text ('global','organization','site','user','conversation'), organization_id uuid null → organizations cascade, site_id uuid null → sites cascade, user_id uuid null → users cascade, conversation_id uuid null, key text, value jsonb, kind text ('fact','preference','instruction','brand'), source text ('user','agent','import','derived'), confidence numeric(3,2) default 1.00, pinned bool default false, expires_at timestamptz null, created_by uuid null → users set null, created_at, updated_at | unique `(scope, coalesce(organization_id, site_id, user_id, conversation_id), key)`; `(organization_id, scope, updated_at desc)`; `(expires_at)` where not null |
| `ai_site_context` | site_id uuid pk → sites cascade, organization_id uuid, brand_name text, tone text, primary_language text, target_audience text, instructions text default '', extra jsonb default '{}', updated_by uuid null → users set null, updated_at | `(organization_id)` |
| `ai_knowledge_sources` | id uuid pk, collection_id uuid → cascade, organization_id, site_id uuid null, kind text ('page_revision','media','upload','url','manual'), source_ref text, title text, language text, bytes bigint, content_hash text, extracted_tokens int, status text ('pending','extracting','embedded','failed','deleted'), error text, last_indexed_at, created_at, updated_at | unique `(collection_id, kind, source_ref)`; `(status)` where status not in ('embedded','deleted'); `(source_ref)` |
| `ai_knowledge_jobs` | id bigserial pk, organization_id, collection_id uuid cascade, source_id uuid null → cascade, kind text ('index','reindex','delete','refetch'), priority smallint default 100, attempts smallint default 0, status text ('queued','running','done','failed'), error text, queued_at, started_at, finished_at | `(status, priority, queued_at)` where status in ('queued','running'); `(collection_id, queued_at desc)` |

`conversation_id` is written before REQ-103 lands; the foreign key is added by REQ-103's migration
when `ai_conversations` exists, and until then the service rejects an unknown conversation id.
Embedding dimensions are validated at collection creation; a model of another dimension is refused
with the model named, never truncated.

### Events

| Event | Kind | Payload / webhook relevance |
|---|---|---|
| `ai.memory.written` / `ai.memory.deleted` | emitted | scope, key, actor — the answer to "why does the AI know this" |
| `ai.knowledge.source.attached` / `.removed` | emitted | collection, source kind, source ref |
| `ai.knowledge.collection.reindexed` | emitted | chunk count, embedding model — usable as an automation trigger (REQ-003) |
| `page.revision.published`, `media.updated`, `media.deleted` | consumed | reindex the matching source; a delete removes its chunks |
| `ai.airgap.enabled` (REQ-106) | consumed | pause collections whose embedding model is remote and flag them in the UI |

### Acceptance criteria

- [ ] `/ai/memory` lists only rows the caller may read: a second user's `user`-scoped rows are absent for a manager, and an organization member cannot request `global` writes.
- [ ] A `user`-scoped row written by one agent is never returned by `/api/v1/ai/memory` for another user (asserted by an integration test with two sessions).
- [ ] Site context set for site A is present in a chat request scoped to site A and absent in one scoped to site B (asserted by inspecting the assembled context recorded on the request log).
- [ ] Indexing a page revision produces sources, chunks with `heading_path`, and an embedding of the pinned dimension.
- [ ] Publishing a new revision of an indexed page enqueues exactly one reindex job and the search result text follows the new revision.
- [ ] Deleting a media item whose extracted text is indexed removes its source and chunks; a search for a phrase that only existed there returns nothing.
- [ ] `/api/v1/ai/knowledge/search` with `mode=hybrid` returns the chunk whose heading path matches the query even when cosine score is below `min_score`, ranking it above unrelated chunks.
- [ ] `/api/v1/ai/knowledge/ask` returns an answer whose every factual sentence maps to a returned citation; a query with no chunk above threshold answers "not found in the knowledge base" and writes no citation.
- [ ] A URL source that returns a non-2xx status or a non-text content type is refused at Inspect time with the reason, and no source row is created.
- [ ] Every screen has empty, loading and error states with a real action; no placeholder text and no dead button.
- [ ] The context builder drops the oldest unpinned memory rows first when the token budget is exceeded, and records the drop on the request log.
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and the QA walkthrough are green with zero high findings.

### QA plan

The browser walkthrough must: open `/ai/memory`, create one row per scope it covers (bad key first →
field error, then a valid key), pin a row, edit its value, delete it and confirm; open
`/ai/knowledge`, create a collection with a local embedding model, attach a page revision, watch the
job go queued → done in the Sources tab, search it and click a citation through to the page; attach
a URL — first a bad one (Inspect error), then a good one; change the chunk strategy and press
Reindex all, then compare the chunk count; open site context for a site, save it and use Preview to
see it in the assembled context; run one grounded question in chat and open the Sources disclosure;
reload with the network throttled to catch the loading states, and with the API stopped to catch the
error states.

The visual check must see: aligned numeric columns in the sources and jobs tables, no clipped
citation chips, readable status badges for queued/running/failed, the heading path visible on each
search result, no raw i18n keys, no text overlapping the job error column, and a mobile pass
(390×844) over memory, collections and the add-source sheet.

### Slices

1. **Memory and scope rules** — `ai_memory` scopes and rules, `/ai/memory` screen, site context
   table and screen, the `context` builder wired into chat, `ai.memory.*` keys and events.
   *Done when:* a fact written for a site appears in that site's next answer, and a second user's
   `user`-scoped rows are invisible to a manager in both the UI and the API.
2. **Sources and indexing pipeline** — `ai_knowledge_sources`, `ai_knowledge_jobs`, the indexing
   runner in `apps/api`, extraction for page revisions, media text, uploads and URLs, chunking
   strategies with heading paths, `pgvector` writes.
   *Done when:* a page revision, a PDF upload and a URL each index to embedded chunks, and the job
   list shows one row per source with a terminal status.
3. **Retrieval, citations and reindex on change** — vector and hybrid search, rerank hook, search
   and ask endpoints, the citation component on chat surfaces, event-driven reindex, purge on
   source delete.
   *Done when:* a grounded question returns citations that link to the right page, and publishing a
   revision makes the new text findable within one job cycle.
4. **Settings, polish and jobs UI** — collection settings tab, URL inspect, expired-memory purge,
   empty/loading/error states, mobile layouts.
   *Done when:* the four screens pass the visual check at both widths and the acceptance list is
   fully ticked.

### Risks / notes

  retrieval.
- Chunking and embedding settings are effectively data migrations — changing them without a
  reindex leaves mixed embeddings; the UI must warn and the API must mark the collection
  `reindex required`.
- Cite-or-say-nothing is the rule that keeps the feature trustworthy; an answer with no citation
  must not be dressed up as grounded.
- `pgvector` availability is not guaranteed on managed PostgreSQL — the migration fails loudly and
  the UI states that indexing is unavailable rather than offering a button that cannot work.
- Memory is a data-protection surface: `user`-scoped rows and uploaded documents are tenant data,
  so retention, export and deletion (REQ-104 logs, REQ-031 export) must include them.
