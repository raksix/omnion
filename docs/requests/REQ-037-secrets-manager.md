# REQ-037 — Secrets Manager

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core (secrets) + integrations
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

Instead of leaving passwords in `.env` files:

```text
Secrets
├── OpenAI API Key
├── Stripe Secret
├── SMTP Password
├── LDAP Password
└── AWS Credentials
```

With integrations:

```text
Vault
AWS Secrets Manager
Azure Key Vault
Kubernetes Secrets
```

## Notes

- The secrets-provider pattern from docs/09 §9 (n8n's `secrets-provider-connection`) is prior
  art; secrets must never surface in logs, exports, or AI prompts (docs/06 §18 Data Guard).
