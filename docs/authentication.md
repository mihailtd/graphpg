# Authentication and Row-Level Security

graphpg is an **OAuth 2.0 resource server**, for its GraphQL and REST APIs alike. It does not issue tokens or log users in. Your authorization server does that (Keycloak, Auth0, Entra ID, Okta, Cognito, Supabase Auth, ...). graphpg verifies the access token on each request and makes its claims available as session settings. **PostgreSQL Row-Level Security policies then decide what the request can read and write**, by comparing those claims (user id, email, application roles, tenant, ...) with your data.

graphpg gives claims no meaning. An application `role: editor` claim is data your policies compare, not a PostgreSQL role. Checking claims against the right columns is the schema author's job. The PostgreSQL role a request runs as is fixed by configuration, and tokens never choose it.

Authentication is **off by default**. Every request then runs as graphpg's database user.

## Standards followed

| Concern | Behaviour | Standard |
| :--- | :--- | :--- |
| Token transport | `Authorization: Bearer <token>` only (no query string, no custom headers) | RFC 6750 |
| Token format | JWT access tokens | RFC 7519, RFC 9068 |
| Verification | Signature with an allowed algorithm (never `none`), `iss`, `aud`, `exp`, `nbf` | RFC 8725 |
| Keys | The issuer's JSON Web Key Set, found through OpenID Connect discovery or RFC 8414 metadata, cached and re-fetched when a token names an unknown `kid` (key rotation) | RFC 7517, RFC 8414 |
| Errors | `401` + `WWW-Authenticate: Bearer error="invalid_token"`; `401` + `WWW-Authenticate: Bearer` when a token is required but missing; `400` `invalid_request` for a malformed `Authorization` header | RFC 6750 §3 |

## REST

The same token authenticates `/rest` and `/graphql`, and the same role and session settings reach PostgreSQL, so one set of Row-Level Security policies covers both. REST authentication failures are `application/problem+json` (`code: UNAUTHENTICATED`, status 401, or 400 for a malformed header) and keep the `WWW-Authenticate` challenge. A REST request for a row the policies hide is a 404. With `rbac.enabled`, the resources and operations the connecting role has no privilege for are not served (403) and are left out of the OpenAPI document (one document per container; run another container with another role for another audience, see [Federation and RBAC](federation-and-rbac.md)).

## Configuration

In `graphpg.yaml` (the editor validates it against `graphpg.schema.json`):

```yaml
auth:
  enabled: true
  issuer: https://idp.example.com/realms/acme   # tokens' `iss`; keys discovered from it
  audience: acme-api                            # tokens' `aud` must contain it (a list is fine)
  # jwks_uri: https://idp.example.com/keys        # only when discovery is not available
  # algorithms: [RS256, ES256]                    # default: RS*, PS*, ES256, ES384, EdDSA
  # token_type: at+jwt                            # require RFC 9068's `typ` (many servers send `JWT`)
  # leeway_seconds: 30
  roles:                        # PostgreSQL roles, fixed by deployment (tokens never choose one)
    authenticated: authenticated  # requests with a valid token; omit to run as graphpg's login
    anonymous: anon               # requests without a token; omit to reject them (401)
  session_variables:
    claims_setting: request.jwt.claims   # every claim, as JSON
    prefix: request.jwt.claim.           # one setting per top-level claim (request.jwt.claim.sub)
    mappings:
      org_id: app.current_org_id         # further settings, from a claim name or dot path
```

For HMAC-signed tokens (a shared secret instead of a key set), set the `AUTH_JWT_SECRET` environment variable (at least 32 bytes). Asymmetric keys are preferable.

All authentication settings belong in `graphpg.yaml`, except the shared HMAC secret which is a secret and must be provided via the `AUTH_JWT_SECRET` environment variable.

graphpg checks the configuration at startup and refuses to start if it is invalid, listing every problem. Metadata and keys must be served over `https`, except from `localhost`.

## What the database needs

The database needs no functions, extensions or engine-owned objects. Your migrations create the roles and policies; graphpg only calls PostgreSQL's built-in `set_config`.

```sql
-- graphpg logs in as `authenticator`, which holds no privileges of its own
CREATE ROLE authenticator LOGIN NOINHERIT PASSWORD '...';
CREATE ROLE anon NOLOGIN;
CREATE ROLE authenticated NOLOGIN;
GRANT anon, authenticated TO authenticator;

ALTER TABLE documents ENABLE ROW LEVEL SECURITY;
CREATE POLICY own_document ON documents FOR ALL TO authenticated
  USING (owner_id = current_setting('request.jwt.claims', true)::jsonb ->> 'sub');

-- An application role from the token is data like any other claim
CREATE POLICY editors_see_their_department ON documents FOR SELECT TO authenticated
  USING (current_setting('request.jwt.claim.role', true) = 'editor'
         AND department = current_setting('request.jwt.claim.department', true));
```

If `roles.authenticated` is unset, requests run as graphpg's login role. RLS does not apply to superusers or `BYPASSRLS` roles, and only applies to table owners with `FORCE ROW LEVEL SECURITY`. graphpg warns at startup when the login role bypasses RLS.

Claims are data, never SQL: they are bound as parameters of `set_config`. A claim never replaces `claims_setting` or a mapped setting through `prefix` (a claim named `claims` under the prefix `request.jwt.` is skipped), and a token whose claims contain a NUL character, which PostgreSQL settings cannot hold, is refused (`401`).

Each request runs in a transaction that starts with one statement setting `role` and the claim settings with `set_config(..., true)`. PostgreSQL discards them when the transaction ends, so a pooled connection never carries one request's identity into the next: a request whose client goes away still runs to its commit or rollback, and a connection whose request failed unexpectedly mid-transaction is closed rather than returned to the pool. This covers `/graphql`, `@defer`/`@stream` responses, `/export` and `/import`.

## Why RLS is essential for AI-built applications

When AI coding agents author application code, imperative authorization checks scattered across resolvers, controllers, and background workers become the primary source of cross-tenant data leaks. An agent asked to add a new query or export endpoint easily forgets to append a tenant filter, and its accompanying happy-path test passes without detecting the leak.

With PostgreSQL Row-Level Security, the database kernel evaluates security predicates on every table scan. Even if an agent generates an entirely unfiltered query (`SELECT * FROM documents` or `query { documents { nodes { id title } } }`), PostgreSQL transparently appends the policy condition. Reviewers audit a single declarative SQL policy rather than auditing dozens of application-layer handlers across pull requests.

For an architectural breakdown and test walkthrough, read [Why Row-Level Security Is the Only Reliable Auth for AI-Built Apps](https://mihai.ltd/blog/row-level-security-ai-coding-agents).

## GraphiQL

The GraphiQL IDE at `/graphiql` is for development and pre-production. It is on in dev mode (`--mode dev`) and off in prod. Set `graphql.ui: true` (or `false`) in the config to choose otherwise. When it is off, `/graphiql` returns 404. Its assets are embedded in the binary, so it works offline and needs no external hosts in a Content-Security-Policy.

## Bulk endpoints

- `POST /export`: a query whose single root field is a connection, streamed as NDJSON (or CSV with `"format": "csv"`). It is read as the token's role, so RLS applies.
- `POST /import/{Type}`: NDJSON rows of `Create<Type>Input`, inserted all-or-nothing as the token's role, so `WITH CHECK` policies apply.
