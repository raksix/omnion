# REQ-065 — Identity Providers & SSO

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core (`crates/identity`, `crates/auth`) + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Directory and protocol logins beyond local accounts.

- LDAP authentication and Active Directory authentication (bind + search, group sync).
- OIDC login, SAML login, OAuth2 providers, SCIM provisioning (create/update/deactivate users).
- Provider registry screen: add/edit/test a provider, map attributes, default role on first login.
- SSO → role mapping rules (claim/group → role), with a dry-run preview.
- Identity provider sync status, last sync, error surfacing; per-organization enablement.
- Plugin-declared permissions so an integration can extend the identity surface safely.
