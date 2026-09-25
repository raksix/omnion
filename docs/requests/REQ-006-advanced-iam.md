# REQ-006 — Advanced IAM

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core (`crates/auth`, `crates/permissions`)
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

Beyond LDAP/AD, the identity stack adds:

- OIDC
- OAuth2
- SAML
- WebAuthn
- Passkeys
- MFA
- SCIM
- Session management
- Device management
- IP restrictions
- Login policies
- Password policies
- RBAC
- ABAC
- Custom roles

Example — per-resource permission matrix:

```text
Marketing Manager

Pages:
  read ✓
  create ✓
  update ✓
  delete ✗

Users:
  read ✓
  manage ✗

Billing:
  ✗
```

## Notes

- Builds on the identity/authorization vision in docs/01-VISION.md §2 and
  docs/02-ARCHITECTURE.md (Enterprise side).
- Full design: [`docs/07-IAM.md`](../07-IAM.md) — custom roles, granular permissions,
  hierarchy + inheritance, allow/deny precedence, scopes, ABAC, policy builder, permission
  simulator, service accounts, temporary roles.
