# REQ-005 — Organization / Tenant System

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core (`crates/identity`)
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

A first-class organization/tenant layer:

```text
Omnion
│
├── Acme Corp
│   ├── Marketing
│   ├── HR
│   └── IT
│
├── Company B
│   ├── Site A
│   └── Site B
│
└── Company C
```

Each organization can have its own:

- users
- roles
- sites
- API keys
- billing
- plugins
- settings
