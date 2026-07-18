//! Business-level metrics for VlorQl.
//!
//! [`VlorqMetrics`] holds counters, histograms, and up-down counters
//! that are recorded at key points in the query pipeline.  The metrics
//! are exported via the global OTLP meter provider configured by
//! [`super::init_telemetry`].

use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram, UpDownCounter};

/// Aggregate of all business metrics collected by VlorQl.
///
/// Create one instance via [`VlorqMetrics::new`] (which reads the
/// global meter) and share it across the application as an
/// `Arc<VlorqMetrics>`.
#[derive(Clone)]
pub struct VlorqMetrics {
    /// Total number of queries started (including retries).
    pub query_counter: Counter<u64>,
    /// Histogram of end-to-end query durations in seconds.
    pub query_duration_histogram: Histogram<f64>,
    /// Total number of errors, tagged by error type.
    pub error_counter: Counter<u64>,
    /// Histogram of LLM call durations in seconds.
    pub llm_duration_histogram: Histogram<f64>,
    /// Number of compile-cache hits.
    pub cache_hit_counter: Counter<u64>,
    /// Number of compile-cache misses.
    pub cache_miss_counter: Counter<u64>,
    /// Gauge of currently in-flight queries.
    pub active_queries: UpDownCounter<i64>,
}

impl VlorqMetrics {
    /// Creates a new metrics handle by reading the global meter.
    ///
    /// This **must** be called after [`global::set_meter_provider`] has
    /// been invoked (e.g. after [`super::init_telemetry`]), otherwise
    /// the instruments will be no-ops.
    #[must_use]
    pub fn new() -> Self {
        let meter = global::meter("vlorql");
        Self {
            query_counter: meter.u64_counter("vlorql.queries.total").build(),
            query_duration_histogram: meter.f64_histogram("vlorql.query.duration").build(),
            error_counter: meter.u64_counter("vlorql.errors.total").build(),
            llm_duration_histogram: meter.f64_histogram("vlorql.llm.duration").build(),
            cache_hit_counter: meter.u64_counter("vlorql.cache.hits").build(),
            cache_miss_counter: meter.u64_counter("vlorql.cache.misses").build(),
            active_queries: meter.i64_up_down_counter("vlorql.queries.active").build(),
        }
    }
}

impl Default for VlorqMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::global;

    /// Sets up a noop meter provider and returns the metrics handle.
    fn setup_metrics() -> VlorqMetrics {
        // Use a noop SDK provider so the test never needs a real OTLP endpoint.
        let provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder().build();
        global::set_meter_provider(provider);
        VlorqMetrics::new()
    }

    #[test]
    fn query_counter_does_not_panic() {
        let m = setup_metrics();
        m.query_counter.add(1, &[]);
        m.query_counter.add(5, &[]);
    }

    #[test]
    fn query_duration_histogram_does_not_panic() {
        let m = setup_metrics();
        m.query_duration_histogram.record(0.042, &[]);
        m.query_duration_histogram.record(1.5, &[]);
    }

    #[test]
    fn error_counter_does_not_panic() {
        let m = setup_metrics();
        m.error_counter.add(1, &[opentelemetry::KeyValue::new("error_type", "validation")]);
        m.error_counter.add(1, &[opentelemetry::KeyValue::new("error_type", "llm")]);
    }

    #[test]
    fn llm_duration_histogram_does_not_panic() {
        let m = setup_metrics();
        m.llm_duration_histogram.record(0.3, &[]);
        m.llm_duration_histogram.record(2.1, &[]);
    }

    #[test]
    fn cache_hit_counter_does_not_panic() {
        let m = setup_metrics();
        m.cache_hit_counter.add(1, &[]);
    }

    #[test]
    fn cache_miss_counter_does_not_panic() {
        let m = setup_metrics();
        m.cache_miss_counter.add(1, &[]);
    }

    #[test]
    fn active_queries_updown_does_not_panic() {
        let m = setup_metrics();
        m.active_queries.add(1, &[]); // query starts
        m.active_queries.add(-1, &[]); // query ends
    }

    #[test]
    fn default_creates_usable_instance() {
        let provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder().build();
        global::set_meter_provider(provider);
        let m = VlorqMetrics::default();
        m.query_counter.add(1, &[]);
        // Verify no panic.
    }
}