//! Logs, metrics and traces (`telemetry:` in `graphpg.yaml`).
//!
//! - **Logs** go to standard output as text or JSON lines, filtered by `telemetry.logs.level`
//!   (`RUST_LOG` wins). A line logged during a request carries its `request_id` and, when
//!   traces are exported, its `trace_id`.
//! - **Metrics** ([`metrics`]) are served in the Prometheus format at
//!   `telemetry.metrics.path` and/or pushed over OTLP.
//! - **Traces**: every request runs in an `http.request` span holding `graphql.parse`,
//!   `graphql.plan`, `graphql.compile`, `graphql.execute` / `rest.execute` and `db.operation`.
//!   With the `otel` feature and an OTLP endpoint they are exported, along with the metrics
//!   and the log events ([`otel`]).
pub mod http;
pub mod metrics;
#[cfg(feature = "otel")]
pub mod otel;

use crate::config::models::LogFormat;
use crate::config::TelemetryConfig;
pub use metrics::metrics;

/// Keeps the exporters running; flushes what they hold when dropped.
pub struct TelemetryGuard {
    #[cfg(feature = "otel")]
    providers: otel::Providers,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        #[cfg(feature = "otel")]
        self.providers.shutdown();
    }
}

/// Installs the global subscriber and, with `export`, the OTLP exporters `config` asks for.
///
/// Call it outside the async runtime: the OTLP exporters send from threads of their own with
/// a blocking HTTP client, which can neither be created nor dropped inside the runtime.
pub fn init(config: &TelemetryConfig, export: bool) -> TelemetryGuard {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{EnvFilter, Layer};

    type Base = tracing_subscriber::layer::Layered<EnvFilter, tracing_subscriber::Registry>;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::try_new(&config.logs.level).unwrap_or_else(|err| {
            eprintln!(
                "invalid telemetry.logs.level {:?} ({err}): logging at info",
                config.logs.level
            );
            EnvFilter::new("info")
        })
    });
    #[cfg_attr(not(feature = "otel"), allow(unused_mut))]
    let mut layers: Vec<Box<dyn Layer<Base> + Send + Sync + 'static>> =
        vec![match config.logs.format {
            LogFormat::Text => Box::new(tracing_subscriber::fmt::layer()),
            // Every span of the request is listed, so a line carries its request and trace ids
            LogFormat::Json => Box::new(
                tracing_subscriber::fmt::layer()
                    .json()
                    .flatten_event(true)
                    .with_current_span(false)
                    .with_span_list(true),
            ),
        }];

    if config.metrics.enabled {
        metrics().enable_prometheus();
    }

    #[cfg(feature = "otel")]
    let providers = if export {
        let (otel_layers, providers) = otel::init::<Base>(config);
        layers.extend(otel_layers);
        providers
    } else {
        otel::Providers::default()
    };
    #[cfg(not(feature = "otel"))]
    if export && config.otlp.resolved_endpoint().is_some() {
        eprintln!("telemetry.otlp is set, but this build has no OTLP export (the `otel` feature)");
    }

    tracing_subscriber::registry()
        .with(filter)
        .with(layers)
        .init();

    TelemetryGuard {
        #[cfg(feature = "otel")]
        providers,
    }
}
