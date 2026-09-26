//! OpenTelemetry export (feature `otel`): an OTLP/HTTP span exporter wrapped
//! in a `tracing` layer, so the runtime's `session` / `turn` / `effect` spans
//! reach any OTLP collector.
//!
//! ```ignore
//! use tracing_subscriber::prelude::*;
//! let (layer, _guard) = agent_runtime::otel::layer(None, "my-agent")?;
//! tracing_subscriber::registry().with(layer).init();
//! ```
//!
//! Keep the guard alive for the life of the process: dropping it flushes and
//! shuts the exporter down.

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider};
use opentelemetry_sdk::Resource;
use tracing::Subscriber;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::registry::LookupSpan;

/// Flushes and shuts the tracer provider down on drop.
pub struct OtelGuard {
    provider: SdkTracerProvider,
}

impl OtelGuard {
    pub fn provider(&self) -> &SdkTracerProvider {
        &self.provider
    }
}

impl Drop for OtelGuard {
    fn drop(&mut self) {
        if let Err(e) = self.provider.shutdown() {
            eprintln!("otel shutdown: {e}");
        }
    }
}

/// Build the layer. `endpoint`: OTLP/HTTP traces endpoint (default: the
/// `OTEL_EXPORTER_OTLP_*` environment variables, else
/// `http://localhost:4318/v1/traces`).
pub fn layer<S>(endpoint: Option<&str>, service_name: &str) -> Result<(OpenTelemetryLayer<S, SdkTracer>, OtelGuard), String>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let mut b = SpanExporter::builder().with_http();
    if let Some(e) = endpoint {
        b = b.with_endpoint(e);
    }
    let exporter = b.build().map_err(|e| format!("otlp exporter: {e}"))?;
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(Resource::builder().with_service_name(service_name.to_string()).build())
        .build();
    let tracer = provider.tracer("agent-runtime");
    Ok((tracing_opentelemetry::layer().with_tracer(tracer), OtelGuard { provider }))
}
