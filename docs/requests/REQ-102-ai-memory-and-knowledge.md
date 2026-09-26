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
