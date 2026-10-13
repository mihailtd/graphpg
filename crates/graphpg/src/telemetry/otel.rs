//! OpenTelemetry export over OTLP/HTTP: the request spans, the metrics of [`super::metrics`]
//! and the log events, each to `<endpoint>/v1/<signal>`, tagged with the `service.name` and
//! `service.version` resource (plus `OTEL_RESOURCE_ATTRIBUTES`). Incoming W3C `traceparent`
//! headers continue the caller's trace.
use super::metrics::{
    metrics, Descriptor, CACHE_REQUESTS, DB_DURATION, DURATION_BUCKETS, EVENT_DELIVERIES,
    GRAPHQL_DURATION, HTTP_ACTIVE, HTTP_DURATION, PLANNING_DURATION, POOL_COUNT, POOL_MAX,
    POOL_PENDING,
};
use crate::config::TelemetryConfig;
use opentelemetry::metrics::{Counter, Histogram, Meter, ObservableUpDownCounter, UpDownCounter};
use opentelemetry::KeyValue;
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
use opentelemetry_sdk::Resource;
use std::collections::HashMap;
use tracing_subscriber::Layer;

/// The running exporters: flushed and stopped by [`Providers::shutdown`].
#[derive(Default)]
pub struct Providers {
    tracer: Option<SdkTracerProvider>,
    meter: Option<SdkMeterProvider>,
    logger: Option<SdkLoggerProvider>,
}

impl Providers {
    pub fn shutdown(&mut self) {
        if let Some(provider) = self.tracer.take() {
            let _ = provider.shutdown();
        }
        if let Some(provider) = self.meter.take() {
            let _ = provider.shutdown();
        }
        if let Some(provider) = self.logger.take() {
            let _ = provider.shutdown();
        }
    }
}

/// Targets of the exporter's own HTTP client: their events are not exported as logs, or each
/// export would log another one.
fn exporter_internal(target: &str) -> bool {
    ["hyper", "h2", "reqwest", "tower", "opentelemetry", "rustls"]
        .iter()
        .any(|prefix| target.starts_with(prefix))
}

/// Starts the exporters `config.otlp` asks for and returns the subscriber layers feeding them
/// (spans, log events). With no endpoint nothing is exported.
pub fn init<S>(
    config: &TelemetryConfig,
) -> (Vec<Box<dyn Layer<S> + Send + Sync + 'static>>, Providers)
where
    S: tracing::Subscriber
        + for<'span> tracing_subscriber::registry::LookupSpan<'span>
        + Send
        + Sync,
{
    let mut layers: Vec<Box<dyn Layer<S> + Send + Sync + 'static>> = Vec::new();
    let mut providers = Providers::default();
    let otlp = &config.otlp;
    let Some(endpoint) = otlp.resolved_endpoint() else {
        return (layers, providers);
    };
    let resource = Resource::builder()
        .with_service_name(config.resolved_service_name())
        .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
        .build();
    let headers: HashMap<String, String> = otlp
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let url = |signal: &str| format!("{endpoint}/v1/{signal}");
    let mut exported = Vec::new();

    if otlp.traces {
        match opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(url("traces"))
            .with_headers(headers.clone())
            .build()
        {
            Ok(exporter) => {
                use opentelemetry::trace::TracerProvider as _;
                opentelemetry::global::set_text_map_propagator(
                    opentelemetry_sdk::propagation::TraceContextPropagator::new(),
                );
                let provider = SdkTracerProvider::builder()
                    .with_batch_exporter(exporter)
                    .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
                        config.traces.sample_ratio,
                    ))))
                    .with_resource(resource.clone())
                    .build();
                let tracer = provider.tracer("graphpg");
                layers.push(Box::new(tracing_opentelemetry::layer().with_tracer(tracer)));
                providers.tracer = Some(provider);
                exported.push("traces");
            }
            Err(err) => eprintln!("OTLP trace export disabled: {err}"),
        }
    }

    if otlp.metrics {
        match opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_endpoint(url("metrics"))
            .with_headers(headers.clone())
            .build()
        {
            Ok(exporter) => {
                let reader = PeriodicReader::builder(exporter)
                    .with_interval(otlp.interval.0)
                    .build();
                let provider = SdkMeterProvider::builder()
                    .with_reader(reader)
                    .with_resource(resource.clone())
                    .build();
                use opentelemetry::metrics::MeterProvider as _;
                metrics().attach_otel(Instruments::new(&provider.meter("graphpg")));
                providers.meter = Some(provider);
                exported.push("metrics");
            }
            Err(err) => eprintln!("OTLP metric export disabled: {err}"),
        }
    }

    if otlp.logs {
        match opentelemetry_otlp::LogExporter::builder()
            .with_http()
            .with_endpoint(url("logs"))
            .with_headers(headers)
            .build()
        {
            Ok(exporter) => {
                let provider = SdkLoggerProvider::builder()
                    .with_batch_exporter(exporter)
                    .with_resource(resource)
                    .build();
                let bridge =
                    opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(
                        &provider,
                    )
                    .with_filter(tracing_subscriber::filter::filter_fn(
                        |meta| !exporter_internal(meta.target()),
                    ));
                layers.push(Box::new(bridge));
                providers.logger = Some(provider);
                exported.push("logs");
            }
            Err(err) => eprintln!("OTLP log export disabled: {err}"),
        }
    }

    if !exported.is_empty() {
        eprintln!("exporting {} over OTLP to {endpoint}", exported.join(", "));
    }
    (layers, providers)
}

/// Makes `span` a child of the caller's span named by the W3C `traceparent` header (when
/// there is one), and returns its trace id when it is recorded.
pub fn continue_trace(span: &tracing::Span, headers: &axum::http::HeaderMap) -> Option<String> {
    use opentelemetry::trace::TraceContextExt as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;
    let parent = opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.extract(&HeaderExtractor(headers))
    });
    if parent.span().span_context().is_valid() {
        let _ = span.set_parent(parent);
    }
    let context = span.context();
    let span_context = context.span().span_context().clone();
    (span_context.is_valid() && span_context.is_sampled())
        .then(|| span_context.trace_id().to_string())
}

struct HeaderExtractor<'a>(&'a axum::http::HeaderMap);

impl opentelemetry::propagation::Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

/// The values a pool gauge reads from a pool, with their `db.client.connection.state`.
type PoolReading = fn(&super::metrics::PoolUsage) -> Vec<(i64, Option<&'static str>)>;

/// The OpenTelemetry instruments the metrics are also recorded into.
pub struct Instruments {
    http_duration: Histogram<f64>,
    http_active: UpDownCounter<i64>,
    graphql_duration: Histogram<f64>,
    db_duration: Histogram<f64>,
    planning_duration: Histogram<f64>,
    cache_requests: Counter<u64>,
    event_deliveries: Counter<u64>,
    /// Read from the watched pools at each export.
    _pools: Vec<ObservableUpDownCounter<i64>>,
}

/// The attributes of `values`, labelled as `descriptor` names them; an empty value is no
/// attribute.
fn attributes(descriptor: &Descriptor, values: &[&str]) -> Vec<KeyValue> {
    descriptor
        .labels
        .iter()
        .zip(values)
        .filter(|(_, value)| !value.is_empty())
        .map(|(name, value)| match value.parse::<i64>() {
            Ok(number) if *name == "http.response.status_code" => KeyValue::new(*name, number),
            _ => KeyValue::new(*name, value.to_string()),
        })
        .collect()
}

impl Instruments {
    fn new(meter: &Meter) -> Self {
        let histogram = |d: &Descriptor| {
            meter
                .f64_histogram(d.name)
                .with_unit(d.unit)
                .with_description(d.help)
                .with_boundaries(DURATION_BUCKETS.to_vec())
                .build()
        };
        let pool_gauge = |d: &'static Descriptor, read: PoolReading| {
            meter
                .i64_observable_up_down_counter(d.name)
                .with_unit(d.unit)
                .with_description(d.help)
                .with_callback(move |observer| {
                    for pool in metrics().pools() {
                        for (value, state) in read(&pool) {
                            let mut attributes =
                                vec![KeyValue::new(POOL_COUNT.labels[0], pool.name.clone())];
                            if let Some(state) = state {
                                attributes.push(KeyValue::new(POOL_COUNT.labels[1], state));
                            }
                            observer.observe(value, &attributes);
                        }
                    }
                })
                .build()
        };
        Self {
            http_duration: histogram(&HTTP_DURATION),
            http_active: meter
                .i64_up_down_counter(HTTP_ACTIVE.name)
                .with_unit(HTTP_ACTIVE.unit)
                .with_description(HTTP_ACTIVE.help)
                .build(),
            graphql_duration: histogram(&GRAPHQL_DURATION),
            db_duration: histogram(&DB_DURATION),
            planning_duration: histogram(&PLANNING_DURATION),
            cache_requests: meter
                .u64_counter(CACHE_REQUESTS.name)
                .with_unit(CACHE_REQUESTS.unit)
                .with_description(CACHE_REQUESTS.help)
                .build(),
            event_deliveries: meter
                .u64_counter(EVENT_DELIVERIES.name)
                .with_unit(EVENT_DELIVERIES.unit)
                .with_description(EVENT_DELIVERIES.help)
                .build(),
            _pools: vec![
                pool_gauge(&POOL_COUNT, |p| {
                    vec![(p.idle, Some("idle")), (p.used, Some("used"))]
                }),
                pool_gauge(&POOL_MAX, |p| vec![(p.max, None)]),
                pool_gauge(&POOL_PENDING, |p| vec![(p.pending, None)]),
            ],
        }
    }

    pub fn http_active(&self, delta: i64) {
        self.http_active.add(delta, &[]);
    }

    pub fn http_duration(&self, values: &[&str], seconds: f64) {
        self.http_duration
            .record(seconds, &attributes(&HTTP_DURATION, values));
    }

    pub fn graphql_duration(&self, values: &[&str], seconds: f64) {
        self.graphql_duration
            .record(seconds, &attributes(&GRAPHQL_DURATION, values));
    }

    pub fn db_duration(&self, values: &[&str], seconds: f64) {
        self.db_duration
            .record(seconds, &attributes(&DB_DURATION, values));
    }

    pub fn planning_duration(&self, values: &[&str], seconds: f64) {
        self.planning_duration
            .record(seconds, &attributes(&PLANNING_DURATION, values));
    }

    pub fn cache_requests(&self, values: &[&str]) {
        self.cache_requests
            .add(1, &attributes(&CACHE_REQUESTS, values));
    }

    pub fn event_deliveries(&self, values: &[&str]) {
        self.event_deliveries
            .add(1, &attributes(&EVENT_DELIVERIES, values));
    }
}
