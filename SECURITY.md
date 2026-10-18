# Security policy

## Reporting a vulnerability

Please report vulnerabilities privately, not in public issues: use GitHub's
["Report a vulnerability"](https://github.com/mihailtd/graphpg/security/advisories/new) form on this
repository, or email contact@mihai.ltd. Include what you found, how to reproduce it and the
version affected. Expect an acknowledgement within a few days.

Only the latest release receives fixes.

## Verifying releases

Container images are signed with Sigstore (keyless, by the release workflow) and scanned for known vulnerabilities before they are tagged. Verify one with:

```bash
cosign verify ghcr.io/mihailtd/graphpg:1.2.0 \
  --certificate-identity-regexp '^https://github.com/mihailtd/graphpg/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

## Deployment guidance

- Authentication is off by default. Before exposing graphpg, enable it and set `auth.roles` so requests run as low-privilege roles ([authentication](docs/authentication.md)).
- Let the engine log in as a role that holds no privileges of its own (`NOINHERIT`) and is a member of the roles it switches to. Superusers and `BYPASSRLS` roles are not subject to Row-Level Security.
- Keep GraphiQL and the `--debug` / `--debug-explain` flags off in production: the latter return generated SQL.
- Do not use the default `DATABASE_URL` (a local development URL) outside development.
