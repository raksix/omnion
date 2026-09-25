# REQ-029 — PDF / Document Generation

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core service (documents)
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

Any business object can produce documents:

```text
Quotation
Invoice
Contract
Report
Certificate
Employee Document
```

Templates:

```text
{{ customer.name }}
{{ invoice.number }}
{{ invoice.total }}
```

→ render to PDF.

## Notes

- Template engine mirrors the expression sandboxing requirements of docs/09 (§4); keep
  template evaluation sandboxed.
