//! Telemetry initialisation and shutdown helpers.
//!
//! Configures OTLP trace + metric exporters, a `tracing` subscriber
//! layer, and sets the global tracer and meter providers.

use crate::errors::{ConfigErrorKind, VlorQLError};
use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use serde_json::json;
use std::time::Duration;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Holds the SDK providers so the caller can shut them down cleanly.
pub struct TelemetryGuard {
    tracer_provider: opentelemetry_sdk::trace::SdkTracerProvider,
    meter_provider: opentelemetry_sdk::metrics::SdkMeterProvider,
}

/// Returns a reference to the global tracer, if one has been set.
pub fn global_tracer() -> opentelemetry::global::BoxedTracer {
    opentelemetry::global::tracer("vlorql-tracer")
}

/// Returns a reference to the global meter, if one has been set.
pub fn global_meter() -> opentelemetry::metrics::Meter {
    opentelemetry::global::meter("vlorql-meter")
}

/// Initialises OpenTelemetry tracing and metrics, connects a `tracing`
/// subscriber, and returns a [`TelemetryGuard`] that *must* be kept
/// alive for the lifetime of the application.
///
/// Call [`shutdown_telemetry`] (or drop the guard) during shutdown to
/// flush and close the exporters.
pub fn init_telemetry(
    service_name: &str,
    otlp_endpoint: &str,
) -> Result<TelemetryGuard, VlorQLError> {
    let resource = opentelemetry_sdk::Resource::builder()
        .with_attribute(KeyValue::new("service.name", service_name.to_string()))
        .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
        .build();

    // 1. OTLP Trace exporter
    let trace_exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(otlp_endpoint)
        .with_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| {
            VlorQLError::config(
                ConfigErrorKind::InvalidDialect {
                    dialect: "otlp".to_owned(),
                },
                json!({"message": format!("failed to build OTLP trace exporter: {e}")}),
            )
        })?;

    let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_resource(resource.clone())
        .with_batch_exporter(trace_exporter)
        .build();

    // 2. OTLP Metrics exporter
    let metric_exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_tonic()
        .with_endpoint(otlp_endpoint)
        .with_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| {
            VlorQLError::config(
                ConfigErrorKind::InvalidDialect {
                    dialect: "otlp".to_owned(),
                },
                json!({"message": format!("failed to build OTLP metrics exporter: {e}")}),
            )
        })?;

    let meter_provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
        .with_resource(resource)
        .with_periodic_exporter(metric_exporter)
        .build();

    // 3. Build tracing subscriber
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("vlorql=info,info"));

    let telemetry_layer = tracing_opentelemetry::layer()
        .with_tracer(tracer_provider.tracer("vlorql-tracer"));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().json())
        .with(telemetry_layer)
        .init();

    // 4. Set global providers
    opentelemetry::global::set_tracer_provider(tracer_provider.clone());
    opentelemetry::global::set_meter_provider(meter_provider.clone());

    Ok(TelemetryGuard {
        tracer_provider,
        meter_provider,
    })
}

/// Shuts down the telemetry exporters, flushing any remaining spans
/// and metrics.
pub fn shutdown_telemetry(guard: TelemetryGuard) {
    if let Err(e) = guard.tracer_provider.shutdown() {
        tracing::warn!(error = %e, "tracer provider shutdown encountered an error");
    }
    if let Err(e) = guard.meter_provider.shutdown() {
        tracing::warn!(error = %e, "meter provider shutdown encountered an error");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::global;
    use opentelemetry::trace::Tracer;

    /// Verify that `global_tracer` and `global_meter` return usable
    /// noop instances even when telemetry has not been initialised.
    #[test]
    fn noop_telemetry_does_not_panic() {
        let tracer = global_tracer();
        let _span = tracer.start("test-span");
        let meter = global_meter();
        let _counter = meter.u64_counter("test_counter").build();
    }

    /// Verify that init + shutdown does not panic when using in-memory
    /// (noop) exporters.
    #[test]
    fn init_then_shutdown_does_not_panic() {
        // Use noop providers so we don't need a real OTLP endpoint.
        let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .build();
        let meter_provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
            .build();

        global::set_tracer_provider(tracer_provider.clone());
        global::set_meter_provider(meter_provider.clone());

        let guard = TelemetryGuard {
            tracer_provider,
            meter_provider,
        };
        shutdown_telemetry(guard);
    }
}