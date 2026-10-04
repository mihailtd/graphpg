//! Logging and tracing. Requests run in spans — `graphql.parse`, `graphql.plan`,
//! `graphql.compile`, `graphql.execute` — that are logged, and, with the `otel` feature and
//! `OTEL_EXPORTER_OTLP_ENDPOINT` set, exported as OpenTelemetry spans over OTLP/HTTP.

/// Keeps span export running; flushes pending spans when dropped.
pub struct TelemetryGuard {
    #[cfg(feature = "otel")]
    provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        #[cfg(feature = "otel")]
        if let Some(provider) = self.provider.take() {
            let _ = provider.shutdown();
        }
    }
}

/// Installs the global subscriber: formatted logs (filtered by `RUST_LOG`, `info` by default)
/// and, when configured, OpenTelemetry export.
pub fn init() -> TelemetryGuard {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer());

    #[cfg(feature = "otel")]
    {
        use opentelemetry::trace::TracerProvider as _;
        let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
            .ok()
            .filter(|e| !e.is_empty());
        if endpoint.is_some() {
            // The exporter reads OTEL_EXPORTER_OTLP_* itself
            match opentelemetry_otlp::SpanExporter::builder()
                .with_http()
                .build()
            {
                Ok(exporter) => {
                    let service = std::env::var("OTEL_SERVICE_NAME")
                        .unwrap_or_else(|_| "graphpg".to_string());
                    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
                        .with_batch_exporter(exporter)
                        .with_resource(
                            opentelemetry_sdk::Resource::builder()
                                .with_service_name(service)
                                .build(),
                        )
                        .build();
                    let tracer = provider.tracer("graphpg");
                    registry
                        .with(tracing_opentelemetry::layer().with_tracer(tracer))
                        .init();
                    return TelemetryGuard {
                        provider: Some(provider),
                    };
                }
                Err(err) => eprintln!("OpenTelemetry export disabled: {err}"),
            }
        }
        registry.init();
        TelemetryGuard { provider: None }
    }

    #[cfg(not(feature = "otel"))]
    {
        registry.init();
        TelemetryGuard {}
    }
}
