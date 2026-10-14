# Observability

graphpg reports on itself in the open formats monitoring stacks already read:

- **Metrics** in the Prometheus text format at `/metrics`, and/or pushed over OTLP.
- **Traces** over OTLP. A request carrying a W3C `traceparent` header continues the caller's trace.
- **Logs** on standard output as text or JSON lines, and/or pushed over OTLP. Each line logged during a request carries the request's id and trace id.

These formats plug into Prometheus, any OpenTelemetry collector, Grafana Alloy, the Grafana LGTM stack (Loki, Grafana, Tempo, Mimir), Grafana Cloud, Datadog, Honeycomb and other OTLP backends. Everything is set under `telemetry:` in [`graphpg.yaml`](configuration.md#graphpgyaml) and read at startup.

```yaml
telemetry:
  service_name: orders-api        # service.name (default: OTEL_SERVICE_NAME, else graphpg)
  logs:
    level: info                   # or a filter: info,graphpg=debug (RUST_LOG wins when set)
    format: json                  # text (default) or json
  metrics:
    enabled: true                 # default: on in dev mode, off in prod
    path: /metrics
    listen: 0.0.0.0:9464          # optional: a port of its own, off the API's port
  traces:
    sample_ratio: 1               # share of new traces kept (callers' sampled traces are always kept)
  otlp:
    endpoint: http://otel-collector:4318   # default: OTEL_EXPORTER_OTLP_ENDPOINT, else no export
    headers:
      Authorization: Basic ${GRAFANA_OTLP_TOKEN}
    traces: true
    metrics: true
    logs: true
    interval: 60s                 # how often metrics are pushed
```

## Metrics

Metric names follow the [OpenTelemetry semantic conventions](https://opentelemetry.io/docs/specs/semconv/). The Prometheus names are the ones an OTLP backend gives the pushed metrics (Mimir, or Prometheus' OTLP receiver): dots become underscores, seconds get `_seconds` and counters `_total`. One dashboard therefore works whether Prometheus scrapes graphpg or graphpg pushes to an OTLP backend.

| Prometheus name | OpenTelemetry name | Type | Labels |
| :--- | :--- | :--- | :--- |
| `http_server_request_duration_seconds` | `http.server.request.duration` | histogram | `http_request_method`, `http_route`, `http_response_status_code` |
| `http_server_active_requests` | `http.server.active_requests` | gauge | |
| `graphpg_graphql_operation_duration_seconds` | `graphpg.graphql.operation.duration` | histogram | `graphql_operation_type`, `graphql_operation_name`, `error_type` (the first error's `extensions.code`) |
| `db_client_operation_duration_seconds` | `db.client.operation.duration` | histogram | `db_system_name`, `graphpg_operation_type` (`query`, `mutation`), `error_type` (the SQLSTATE) |
| `db_client_connection_count` | `db.client.connection.count` | gauge | `db_client_connection_pool_name` (`primary`, `replica_1`, ...), `db_client_connection_state` (`idle`, `used`) |
| `db_client_connection_max` | `db.client.connection.max` | gauge | `db_client_connection_pool_name` |
| `db_client_connection_pending_requests` | `db.client.connection.pending_requests` | gauge | `db_client_connection_pool_name` |
| `graphpg_planning_duration_seconds` | `graphpg.planning.duration` | histogram | `graphpg_api` (`graphql`, `rest`): planning and compiling into SQL, on cache misses |
| `graphpg_cache_requests_total` | `graphpg.cache.requests` | counter | `graphpg_cache_name` (`document`, `operation`), `graphpg_cache_result` (`hit`, `miss`) |
| `graphpg_event_deliveries_total` | `graphpg.event.deliveries` | counter | `graphpg_event_trigger` (the trigger or schedule), `graphpg_event_outcome` (`delivered`, `retry`, `dead`, `unknown`); see [event triggers](events.md) |
| `graphpg_subscription_active` | `graphpg.subscription.active` | gauge | subscriptions open on the instance; see [subscriptions](subscriptions.md) |
| `graphpg_subscription_round_duration_seconds` | `graphpg.subscription.round.duration` | histogram | how long a run of every subscription takes (every `subscriptions.interval`) |
| `graphpg_subscription_results_total` | `graphpg.subscription.results` | counter | `graphpg_subscription_result` (`data`, `error`): results sent because they changed |
| `graphpg_build_info` | | gauge | `version` |
| `process_start_time_seconds` | | gauge | |

A database operation is one query statement, or the whole transaction of a mutation, retries included. Histograms use the buckets the HTTP conventions recommend, from 5 ms to 10 s.

**Labels stay bounded.**
- `http_route` is the route template (`/rest/{resource}/{key}`), never the raw path. Requests that match no route have an empty route.
- Methods outside the standard ones are `_OTHER`.
- GraphQL operation names come from clients, so only the first 200 distinct names get a label of their own; later ones are `_OTHER`. A document that fails validation has no operation name.

Some useful queries:

```promql
# Requests per second by route
sum by (http_route) (rate(http_server_request_duration_seconds_count[5m]))
# p95 latency of each GraphQL operation
histogram_quantile(0.95, sum by (le, graphql_operation_name) (rate(graphpg_graphql_operation_duration_seconds_bucket[5m])))
# Share of the pool in use
sum(db_client_connection_count{db_client_connection_state="used"}) / sum(db_client_connection_max)
```

**Scraping.**
- **Prometheus** scrapes `metrics.path`. With `metrics.listen` the endpoint moves to a listener of its own, which keeps it off a public port, and the API's port no longer serves it.
- **Kubernetes with Grafana Alloy:** annotate the pod (`k8s.grafana.com/scrape: "true"`, `k8s.grafana.com/metrics.portNumber: "9464"`).
- **Prometheus Operator:** use a `PodMonitor`.

When metrics are both scraped and pushed over OTLP, they reach the backend twice. Pick one route.

## Traces

Each request runs in a server span named after its route, `GET /rest/{resource}` or `POST /graphql`. Child spans inside it:

- **GraphQL:** `graphql.parse`, `graphql.plan`, `graphql.compile`, `graphql.execute`.
- **REST:** `rest.execute`.
- **Both:** one `postgresql` client span per database operation, with `db.system.name`.

Spans carry the request's `request_id` (the `X-Request-Id` header). A failed request (5xx) or database operation is marked as an error. Tempo's service graph shows PostgreSQL as a node below graphpg.

`traceparent` headers ([W3C Trace Context](https://www.w3.org/TR/trace-context/)) are honored: graphpg's spans join the caller's trace and follow its sampling decision. `traces.sample_ratio` applies to traces that start at graphpg.

Statements also carry [sqlcommenter](https://google.github.io/sqlcommenter/) comments naming the GraphQL operation or REST resource (`database.sql_comments`). These show up in `pg_stat_activity`, slow-query logs and `auto_explain`. The comments do not include the trace id: a value that changes per request would defeat prepared-statement caching.

## Logs

`logs.format: json` writes one JSON object per line. Lines logged during a request list the request's spans, with its `request_id` and, when traces are exported, its `trace_id`. In Grafana, a Loki derived field on `"trace_id":"(\w+)"` links each line to its trace in Tempo.

With `otlp.logs`, log events are also pushed over OTLP with their trace context attached. Do this where nothing collects standard output already: in Kubernetes, Alloy or another agent usually collects pod logs, and pushing too would store each line twice.

## OTLP export

graphpg exports over OTLP/HTTP with protobuf encoding, to `<endpoint>/v1/traces`, `/v1/metrics` and `/v1/logs`. Every signal carries `service.name`, `service.version` and anything in `OTEL_RESOURCE_ATTRIBUTES` (`deployment.environment.name=prod,service.namespace=shop`). The standard `OTEL_EXPORTER_OTLP_*` variables also work: `OTEL_EXPORTER_OTLP_ENDPOINT` when `otlp.endpoint` is unset, and `OTEL_EXPORTER_OTLP_HEADERS` for headers. Credentials belong in the environment: write `${NAME}` in `otlp.headers`.

| Backend | `otlp.endpoint` |
| :--- | :--- |
| OpenTelemetry Collector, Grafana Alloy | `http://<collector>:4318` |
| `grafana/otel-lgtm` (all of LGTM in one container, for development) | `http://localhost:4318` |
| Grafana Cloud | The OTLP endpoint of the stack (`https://otlp-gateway-<region>.grafana.net/otlp`), with `Authorization: Basic <instance id:token, base64>` |

The container image includes OTLP export. A binary built with `--no-default-features` leaves it out (the `otel` feature) and serves Prometheus metrics only.

Try the whole stack locally:

```bash
docker run -d -p 3000:3000 -p 4318:4318 grafana/otel-lgtm
```

Then set `telemetry.otlp.endpoint: http://localhost:4318` and open Grafana at http://localhost:3000.
