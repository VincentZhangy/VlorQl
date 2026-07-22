//! OpenTelemetry integration for VlorQl.
//!
//! Provides a simple `init_telemetry` / `shutdown_telemetry` lifecycle
//! and accessors for the global tracer and meter instances.

mod init;
mod metrics;

pub use init::{
    TelemetryGuard, global_meter, global_tracer, init_console_logging, init_telemetry,
    shutdown_telemetry,
};
pub use metrics::VlorqMetrics;
