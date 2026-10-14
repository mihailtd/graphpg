# Subscriptions (live queries)

A subscription is a query whose result keeps coming: graphpg sends it once, then again each time it changes, until the client stops. Any query field can be subscribed to, with its arguments, relations, filters, aggregates and functions, and Row-Level Security applies as it does to the query.

```graphql
subscription OrderStatus($id: Int!) {
  order(id: $id) {
    status
    courier { name position }
  }
}
```

- [How it works](#how-it-works)
- [Quick start](#quick-start)
- [What is sent](#what-is-sent)
- [Clients and transports](#clients-and-transports)
- [Authentication and Row-Level Security](#authentication-and-row-level-security)
- [What it costs](#what-it-costs)
- [Scaling: replicas and instances for subscriptions](#scaling-replicas-and-instances-for-subscriptions)
- [Behind Apollo Router](#behind-apollo-router)
- [Operating it](#operating-it)
- [Configuration reference](#configuration-reference)
- [What it does not do](#what-it-does-not-do)

## How it works

1. **The schema gets a `Subscription` type** with the `Query` type's root fields: same names, arguments and types. A subscription selects one of them, as GraphQL requires, and is planned, limited and compiled to one SQL statement exactly as the query would be.
2. **Every `interval` (1 s by default), each instance runs its subscriptions again** and sends the results that differ from the last one it sent. Nothing in the database has to tell graphpg what changed, so every change is seen, whatever made it: another application, a function, a policy reading a membership table, a foreign table, the passing of `now()`.
3. **Subscriptions are run in batches.** Those compiled to the same SQL and running with the same claims are one statement: the query joined to the list of their parameters, which returns only the results whose hash changed. Identical subscriptions are run once. Different users' statements are run one after another in one transaction, each with its own claims.

Nothing is installed in the database: no trigger, no `NOTIFY`, no replication slot, no extension. It works with every managed PostgreSQL and behind transaction-mode poolers. The design, and the alternatives considered, are in the decision record of [#27](https://github.com/mihailtd/graphpg/issues/27).

## Quick start

```yaml
subscriptions:
  enabled: true
```

Then subscribe, here with curl over [GraphQL over SSE](#graphql-over-sse):

```bash
curl -N http://localhost:4000/graphql \
  -H 'content-type: application/json' -H 'accept: text/event-stream' \
  -d '{"query": "subscription { order(id: 1) { status } }"}'
```

```
event: next
data: {"data":{"order":{"status":"PAID"}}}

event: next
data: {"data":{"order":{"status":"SHIPPED"}}}
```

In GraphiQL (`graphql.ui`), run a `subscription` operation: results replace each other as they change, until you stop it.

Turning subscriptions on adds the `Subscription` type to the schema: re-export a pinned schema (`graphpg schema export`).

## What is sent

- **The whole result, each time it changes.** The first message is the current result. After that a message is sent only when the result differs from the last one sent: an update that leaves the selected fields as they were sends nothing. Each message is a complete GraphQL response (`{"data": ...}`), so the client replaces what it shows.
- **The latest state, not every state.** Changes made between two runs arrive together as one result, and a value that changes and changes back between them sends nothing. A subscription tells you what the data *is*. To receive every change, one by one, use [event triggers](events.md).
- **Errors are results too.** A run that fails because of the data (a function raising, a policy refusing) sends `{"data": null, "errors": [...]}` once, as the query would answer; the subscription keeps running and sends the data again once the query succeeds. A failure that may not last (the database unreachable, a conflict with recovery on a replica) sends nothing, and the next run tries again.
- **A slow client gets the latest result.** Results waiting for a client are capped; while a client is behind, a newer result is sent once it catches up, rather than every one in between.
- `@defer` and `@stream` are not supported in subscriptions.

## Clients and transports

The `/graphql` endpoint speaks the three protocols GraphQL clients use for subscriptions:

| Transport | Request | Used by |
| :--- | :--- | :--- |
| WebSocket, `graphql-transport-ws` | `GET /graphql` upgraded, subprotocol `graphql-transport-ws` | the `graphql-ws` library, Apollo Client (`GraphQLWsLink`), urql, Relay, Apollo Router (to subgraphs) |
| GraphQL over SSE | `POST` (or `GET`) `/graphql` with `Accept: text/event-stream` | the `graphql-sse` library, curl, `EventSource`-style clients, GraphiQL |
| Multipart HTTP | `POST /graphql` with `Accept: multipart/mixed;subscriptionSpec="1.0"` | Apollo Client over HTTP, Apollo Router (to clients) |

A subscription sent with `Accept: application/json` alone is answered `406 Not Acceptable`: it needs one of these.

### WebSocket

[`graphql-transport-ws`](https://github.com/enisdenjo/graphql-ws/blob/master/PROTOCOL.md), the protocol of the `graphql-ws` library (the older `subscriptions-transport-ws` protocol, `graphql-ws` subprotocol, is refused):

```ts
import { createClient } from "graphql-ws";

const client = createClient({
  url: "wss://api.example.com/graphql",
  connectionParams: async () => ({ Authorization: `Bearer ${await getToken()}` }),
});

for await (const result of client.iterate({
  query: "subscription($id: Int!) { order(id: $id) { status } }",
  variables: { id: 1 },
})) {
  render(result.data);
}
```

- The access token is read from `connection_init`'s payload: `Authorization` (`"Bearer …"`), or `token`, at its top or under `headers` or `connectionParams` (a token without a scheme is a bearer token). Without one there, the upgrade request's `Authorization` header is used (server-side clients can set it).
- Queries and mutations can be sent over the same socket; each gets one `next`, then `complete`.
- Every `keep_alive` the server sends a `ping` (clients answer `pong`), so proxies keep idle sockets open.
- Close codes: `4400` invalid message, `4401` subscribe before `connection_ack`, `4403` invalid or expired token, `4406` wrong subprotocol, `4408` no `connection_init` within `init_timeout`, `4409` an id already in use, `4429` a second `connection_init`.

### GraphQL over SSE

The [`graphql-sse`](https://github.com/enisdenjo/graphql-sse/blob/master/PROTOCOL.md) protocol's *distinct connections* mode: one request per subscription (HTTP/2 multiplexes them on one connection), `next` events, then `complete` when it ends. The token is the request's `Authorization` header.

```ts
import { createClient } from "graphql-sse";

const client = createClient({
  url: "https://api.example.com/graphql",
  headers: async () => ({ Authorization: `Bearer ${await getToken()}` }),
});
```

- A rejected request (an invalid document, a limit) is answered with its HTTP status and a JSON body, not a stream.
- A query or mutation sent with `Accept: text/event-stream` (and no JSON media type) is answered as one `next` event, then `complete`.
- Idle streams get a comment line (`:`) every `keep_alive`.

### Multipart HTTP

Apollo's [multipart subscriptions](https://www.apollographql.com/docs/graphos/routing/operations/subscriptions/multipart-protocol): each result is a JSON part `{"payload": {"data": ...}}`, heartbeats are empty parts (`{}`). Apollo Client uses it over its plain HTTP link, with no extra setup.

## Authentication and Row-Level Security

- **The token is checked once, when subscribing**, and the subscription runs with its claims, as a query with that token would. Each run sets the role and claims for that user's statements and resets them before the next user's, so policies see exactly that user, as in a request.
- **A subscription ends when its token expires** (`exp`): the WebSocket is closed with `4403`, an SSE or multipart stream gets an error with the code `TOKEN_EXPIRED` and ends. Subscribe again with a new token; the first message is the current result, so nothing is missed.
- **Different users, different results.** Two users subscribing to the same query each get what their policies let them see; a change to one user's rows sends nothing to the other.
- **Shared when claims cannot matter.** When everything a query reads is a table without Row-Level Security (or a materialized view, or a foreign table) and it calls no function, the claims cannot change its result. Then the subscriptions of every user to that query are run as one statement, with the role alone. A view, a function or a table with Row-Level Security keeps each user's claims. This is looked up in the catalog and again after a schema reload.

## What it costs

The work is **constant over time**: every `interval`, the subscriptions' queries run again whether or not anything changed, and the database does the same work whether one row or a million rows were written. It does not depend on how many tables there are or how much is written; it depends on what is subscribed.

Per run, the database executes each distinct `(query, claims, variables)` once:

| Who subscribes | Statements per run | Query executions per run |
| :--- | :--- | :--- |
| 1,000 clients, same query and variables, no Row-Level Security (a public dashboard) | 1 | 1, sent to 1,000 clients |
| 1,000 clients, same query, different variables, no Row-Level Security | 10 (`batch`: 100 subscriptions per statement) | 1,000, one per set of variables |
| 1,000 users, same query on a table with Row-Level Security | 1,000, in a few transactions | 1,000 |
| No subscriptions | 0 | 0 |

Batching saves the round trips, planning and transfer, and an unchanged result never leaves the database (only its hash is compared). It does not save execution: a statement still runs the query once per distinct set of parameters. So size it like a load of that many queries per `interval`: 1,000 users each watching a small indexed query every second is about 1,000 small reads per second, a few percent of what one database serves; tens of thousands of users, or expensive queries, need replicas.

To keep it bounded:

- **`interval`**: the latency of a change, and the rate of the load. 1 s is a good default; 5 s divides the load by five.
- **The plan limits** (`limits.depth`, `limits.cost`, page sizes) apply to subscriptions as to queries, and `limits.statement_timeout` to each run.
- **`max_per_instance` and `max_per_subject`** cap the subscriptions an instance holds, and one client holds.
- **Indexes**: a subscription is a query run often. [`/debug/statements`](configuration.md) and `graphpg advise --statements` show the statements that cost the most, subscriptions included (tagged `graphql_type='subscription'` by sqlcommenter).

## Scaling: replicas and instances for subscriptions

**Subscriptions read from replicas.** With `database.replicas`, subscriptions run on a standby by default (`source: replica`), and fall back to the primary when none is available. A result read on a replica is behind by the replica's lag, which is small next to the interval. A run cancelled by a conflict with recovery on the standby is retried at the next interval. `LISTEN` (schema reloads) stays on the primary, where it works.

**Instances for subscriptions alone.** An instance holds its subscriptions in memory only: a client that reconnects anywhere gets the full result first. So subscriptions can be served by their own instances, scaled on their own, reading from replicas, while other instances serve the API:

```yaml
# graphpg.yaml of the instances that serve subscriptions
database:
  replicas:
    - ${REPLICA_URL}            # subscriptions run here; the primary only loads the schema
  pool_size: 4
subscriptions:
  enabled: true
  source: replica
  concurrency: 4
events:
  enabled: false                # the API's instances deliver events
```

- Route subscriptions to them at the load balancer: WebSocket upgrades (`Upgrade: websocket`), and requests accepting `text/event-stream` or `multipart/mixed`, on `/graphql`.
- Raise the load balancer's idle timeout above `keep_alive`, and turn off response buffering for SSE and multipart (graphpg sends `X-Accel-Buffering: no` for Nginx).
- No sticky sessions are needed.
- Identical subscriptions are shared within an instance, not across instances: 1,000 viewers of one dashboard spread over 4 instances are 4 executions per run.
- The instances serving the rest of the API can leave `subscriptions` off, or on: the setting only adds the `Subscription` type and the WebSocket upgrade.

The database's work is the same; what changes is where it runs. Replicas scale out, the primary does not.

## Behind Apollo Router

graphpg's subgraph serves subscriptions to the router over WebSocket (the router's `graphql_ws` protocol is `graphql-transport-ws`), and the router serves them to clients over multipart HTTP:

```yaml
# router.yaml
subscription:
  enabled: true
  mode:
    passthrough:
      subgraphs:
        graphpg:
          path: /graphql
          protocol: graphql_ws
headers:
  subgraphs:
    graphpg:
      request:
        - propagate:
            named: authorization
```

The router puts the propagated token in `connection_init`'s `connectionParams`, which graphpg reads. Fields of other subgraphs in a subscription's result are resolved by the router for each message.

## Operating it

Metrics (Prometheus and OTLP, see [observability](observability.md)):

| Metric | Type | |
| :--- | :--- | :--- |
| `graphpg_subscription_active` | gauge | subscriptions open on the instance |
| `graphpg_subscription_round_duration_seconds` | histogram | how long a run of every subscription takes: close to `interval` means the instance or database cannot keep up |
| `graphpg_subscription_results_total` | counter | results sent, by `graphpg_subscription_result` (`data`, `error`) |

- A run's statements are tagged by sqlcommenter with the operation's name and `graphql_type='subscription'`, so `pg_stat_statements` and `/debug/statements` show their cost.
- Refused subscriptions answer `TOO_MANY_SUBSCRIPTIONS` (`503` for the instance's limit, `429` for a client's), and close nothing else.
- After a schema reload, running subscriptions keep their compiled query; new ones use the new schema.

## Configuration reference

```yaml
subscriptions:
  enabled: false        # adds the Subscription type and the transports
  interval: 1s          # how often subscriptions run again (at least 50ms)
  source: replica       # replica (one of database.replicas, else the primary) | primary
  max_per_instance: 10000
  max_per_subject: 100  # per client: the token's `sub`, else the address it connects from
  batch: 100            # subscriptions run in one statement at most
  concurrency: 4        # connections a run uses at once
  keep_alive: 15s       # WebSocket pings, SSE comments, multipart heartbeats on idle streams
  init_timeout: 10s     # time a WebSocket client has to send connection_init
```

Like every setting, these can differ per mode (`dev:` / `prod:`) and read the environment (`enabled: ${LIVE_QUERIES:-false}`).

## What it does not do

- **React to a change at once.** A change is seen at the next run, within `interval`. Waking runs early on a change (`NOTIFY` from the tables you choose) is a possible later addition, on top of this design; see [#27](https://github.com/mihailtd/graphpg/issues/27).
- **Stream each change.** A subscription sends states, not events. Every insert, update and delete, delivered at least once: [event triggers](events.md).
- **REST streams.** The REST API has no streaming equivalent yet.
