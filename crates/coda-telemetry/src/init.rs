// SPDX-License-Identifier: AGPL-3.0-only

//! Subscriber construction and optional OTLP trace export.

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing::Subscriber;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{EnvFilter, fmt};

const DEFAULT_FILTER_DIRECTIVE: &str = "info";
const SERVICE_NAME: &str = "coda";

/// Telemetry settings. The default exports nothing and captures no content.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TelemetryConfig {
    /// OTLP/HTTP endpoint to export spans to; `None` disables export.
    pub otlp_endpoint: Option<String>,
    /// Reserved for content attributes; not yet read.
    pub capture_content: bool,
}

/// Why telemetry setup failed.
#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    /// The `RUST_LOG` directive could not be parsed.
    #[error("invalid log filter directive: {0}")]
    Filter(#[from] tracing_subscriber::filter::ParseError),
    /// The OTLP span exporter could not be built.
    #[error("failed to build the OTLP span exporter: {0}")]
    Exporter(#[from] opentelemetry_otlp::ExporterBuildError),
    /// A global tracing subscriber was already set.
    #[error("a global tracing subscriber is already installed")]
    AlreadyInstalled(#[from] tracing::subscriber::SetGlobalDefaultError),
}

/// A subscriber that is not yet installed, with the provider backing its exporter.
pub struct Built {
    /// The layered subscriber, ready to install.
    pub subscriber: Box<dyn Subscriber + Send + Sync>,
    /// True only when an OTLP exporter was created.
    pub exporting: bool,
    provider: Option<SdkTracerProvider>,
}

/// Flushes and shuts down the tracer provider on drop.
#[must_use = "dropping the guard shuts telemetry down immediately"]
pub struct TelemetryGuard {
    provider: Option<SdkTracerProvider>,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.provider.take() {
            // Nowhere left to report a shutdown failure during drop.
            let _ = provider.shutdown();
        }
    }
}

/// Builds the layered subscriber without installing it globally.
pub fn build_subscriber(config: &TelemetryConfig) -> Result<Built, TelemetryError> {
    let filter = filter_from(std::env::var(EnvFilter::DEFAULT_ENV).ok().as_deref())?;

    let provider = config
        .otlp_endpoint
        .as_deref()
        .map(build_provider)
        .transpose()?;
    let otel_layer = provider
        .as_ref()
        .map(|p| tracing_opentelemetry::layer().with_tracer(p.tracer(SERVICE_NAME)));

    let subscriber = tracing_subscriber::registry()
        .with(otel_layer)
        .with(filter)
        .with(fmt::layer());

    Ok(Built {
        subscriber: Box::new(subscriber),
        exporting: provider.is_some(),
        provider,
    })
}

/// Builds the subscriber and installs it as the global default.
#[must_use = "the guard must be held for the life of the program to flush spans"]
pub fn init(config: &TelemetryConfig) -> Result<TelemetryGuard, TelemetryError> {
    let Built {
        subscriber,
        provider,
        ..
    } = build_subscriber(config)?;
    tracing::subscriber::set_global_default(subscriber)?;
    Ok(TelemetryGuard { provider })
}

/// Uses the default directive only when none is given; a bad directive is an error.
fn filter_from(directive: Option<&str>) -> Result<EnvFilter, TelemetryError> {
    Ok(EnvFilter::try_new(
        directive.unwrap_or(DEFAULT_FILTER_DIRECTIVE),
    )?)
}

fn build_provider(endpoint: &str) -> Result<SdkTracerProvider, TelemetryError> {
    let exporter = SpanExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .build()?;
    Ok(SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(Resource::builder().with_service_name(SERVICE_NAME).build())
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_endpoint_no_exporter_is_built() {
        // Arrange
        let config = TelemetryConfig::default();

        // Act
        let built = build_subscriber(&config).expect("subscriber builds");
        tracing::subscriber::with_default(built.subscriber, || {
            let _span = tracing::info_span!("probe").entered();
        });

        // Assert
        assert!(!built.exporting);
        assert!(built.provider.is_none());
    }

    #[test]
    fn unset_directive_falls_back_to_default() {
        // Arrange
        let directive = None;

        // Act
        let result = filter_from(directive);

        // Assert
        assert!(result.is_ok());
    }

    #[test]
    fn valid_directive_is_accepted() {
        // Arrange
        let directive = Some("info");

        // Act
        let result = filter_from(directive);

        // Assert
        assert!(result.is_ok());
    }

    #[test]
    fn invalid_directive_is_a_filter_error() {
        // Arrange
        let directive = Some("not a [valid");

        // Act
        let result = filter_from(directive);

        // Assert
        assert!(matches!(result, Err(TelemetryError::Filter(_))));
    }
}
