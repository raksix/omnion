# REQ-088 — Core Node Families

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/workflows`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

The nodes every workflow needs.

- Flow control: If, Switch, Filter, Merge, Split-in-batches, Loop-over-items, Wait-for-multiple.
- Code nodes: JavaScript and Python with static validation and result-shape validation.
- HTTP Request node (auth, pagination, retry, binary), SSH tunnel helper, file-system helper, data-table helper.
- Deduplication helper; binary data helper; date/time and crypto helpers.
- Error-handling nodes: Stop-and-error, Continue-on-fail, Error trigger.
