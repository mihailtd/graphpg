# Extending graphpg

graphpg has no custom resolvers and no plugins. This page says why, and how to add each kind of behavior instead. It is the decision record of [#31](https://github.com/mihailtd/graphpg/issues/31).

## Why there are no custom resolvers or plugins

graphpg answers every GraphQL request and REST call with **one SQL statement**, planned and bounded before it runs, and executed inside the database's transaction and Row-Level Security policies. That is where its speed and its guarantees come from.

A resolver running in graphpg's own process would break this:

- **It is not part of the statement.** Each resolved field becomes another round trip, run once per row unless every resolver batches its work (the N+1 problem graphpg exists to remove). Limits estimated on the plan no longer bound the request.
- **It runs outside the transaction and outside RLS.** A resolver's writes do not roll back with the statement's, and what it reads is not filtered by the policies the request runs under.
- **A plugin runtime is a product of its own.** It needs a language (WASM, JavaScript, native libraries), a stable API, versioning and sandboxing, and every plugin makes each upgrade riskier.

graphpg stays a generic engine: what it serves comes from the database. Behavior is added where it can keep those guarantees.

## Where each kind of behavior goes

| You need | Do this | Guide |
| :--- | :--- | :--- |
| A computed field on a type | A SQL function taking the row: `CREATE FUNCTION full_name(u users) RETURNS text ...` becomes `User.fullName` | [Computed columns](schema-mapping.md#functions-of-a-row-computed-columns) |
| A custom query or mutation | A SQL function: `STABLE` ones are queries, `VOLATILE` ones mutations, also served at `/rest/rpc/<name>` | [Functions](schema-mapping.md#functions) |
| Validation and business rules | Constraints, triggers and RLS policies; their violations become typed error codes | [Errors](graphql-api.md#errors), [Authentication](authentication.md) |
| Side effects: emails, payments, calls to other services | Event triggers: signed webhooks on row changes, captured in the change's transaction, and on schedules | [Event triggers](events.md) |
| Data that updates on screens as it changes | Subscriptions: any query field, its result sent again when it changes, under the same policies | [Subscriptions](subscriptions.md) |
| Background jobs | A function of your own that records the job and enqueues it, for example with pgmq, in one transaction | [Message queues](extensions.md#message-queues-pgmq) |
| Data from another database or from files | Foreign data wrappers, joined to your tables in the same statement | [Other databases and files](foreign-data.md) |
| Renaming, hiding, describing | `graphpg.yaml` | [Configuration](configuration.md) |
| Logic that cannot live in SQL: a third-party API, heavy computation | A service of your own beside graphpg, composed at the gateway with Apollo Federation (graphpg serves a subgraph) | [Federation](federation-and-rbac.md) |

The last row is how other engines' "actions" and "remote schemas" are done here. Your service owns its fields, written in any language, and the gateway composes it with graphpg's subgraph into one API. graphpg's part of each request is still one statement.

## Not planned

- **Custom resolvers and plugins** ([#30](https://github.com/mihailtd/graphpg/issues/30)), for the reasons above.
